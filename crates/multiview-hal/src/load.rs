//! Live per-device load model + the vendor probe seam (ADR-0017).
//!
//! Where [`crate::capability`]/[`crate::cost`]/[`crate::planner`] model what a
//! device *can* do and what *fits*, this module models what a device is *doing
//! right now*: a [`DeviceLoad`] snapshot per GPU (compute-busy, VRAM used/total,
//! per-engine encoder/decoder utilisation, NVENC concurrent-session count). It
//! is the fourth seam in `multiview-hal`, beside the presence-detection
//! [`crate::probe`], and it mirrors that module's discipline exactly:
//!
//! - The **pure load model** ([`DeviceId`], [`DeviceLoad`], [`Vendor`]) always
//!   compiles, with no native deps.
//! - Every field a vendor cannot report is an [`Option`] — "unknown" is a
//!   first-class state, never a fabricated zero
//!   ([gpu-monitoring §2.5](../../../docs/research/gpu-monitoring-and-scheduling.md)).
//!   The selection policy in [`crate::select`] drops an unknown term and
//!   redistributes its weight; it never invents a metric.
//! - The vendor seam is the injectable [`LoadProbe`] trait (mirroring
//!   [`crate::probe::DeviceProbe`]). A [`LoadPoller`] wraps a probe with the
//!   bounded off-hot-path polling contract.
//! - Real vendor probes are feature-gated behind the existing `cuda` / `vaapi`
//!   / `qsv` features. The NVIDIA path (NVML via the runtime-loaded
//!   `nvml-wrapper`) initialises-or-returns-[`LoadSample::Unavailable`]
//!   gracefully on a host with no NVIDIA device — never a panic.
//!
//! See [gpu-monitoring-and-scheduling](../../../docs/research/gpu-monitoring-and-scheduling.md)
//! and ADR-0017.

/// The vendor family of a device, used to label telemetry and to know which
/// per-engine signals to expect.
///
/// Distinct from [`crate::Stage`] (the pipeline stage) and from
/// [`multiview_core::traits::BackendKind`] (the backend implementation): a
/// [`Vendor`] is the *physical silicon family* a [`DeviceLoad`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Vendor {
    /// NVIDIA (NVML; per-engine enc/dec util + NVENC session ceiling).
    Nvidia,
    /// Intel (i915 PMU / DRM fdinfo; per-engine enc/dec util).
    Intel,
    /// AMD (amdgpu sysfs + DRM fdinfo; enc/dec merged from VCN4).
    Amd,
    /// Apple (no public per-engine util; unified memory + thermal only).
    Apple,
}

impl Vendor {
    /// A short, stable, lower-case label for telemetry (the `vendor` gauge
    /// label).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Vendor::Nvidia => "nvidia",
            Vendor::Intel => "intel",
            Vendor::Amd => "amd",
            Vendor::Apple => "apple",
        }
    }
}

/// A **stable** device identity — the placement + pin key.
///
/// Per [gpu-monitoring §2.1](../../../docs/research/gpu-monitoring-and-scheduling.md)
/// this is the vendor's stable handle (NVML UUID, PCI bus id, DRM render-node
/// plus PCI, or Metal registryID), **never** the enumeration index, which is
/// unstable across reboots and `CUDA_VISIBLE_DEVICES` reorderings. An operator
/// pin ([`crate::select`]) binds to this.
///
/// The `index` is retained only as a deterministic, never-load-bearing
/// tie-breaker for selection (lowest index wins an exact score tie); identity
/// and equality are defined by `(vendor, stable_id)`.
///
/// `pci_bus_id` is the **non-identity** cross-probe matching key (the same role
/// [`GpuTargetInfo::pci_bus_id`] plays for the wgpu adapter): the stable id is
/// vendor-asymmetric (an NVML UUID on NVIDIA, the PCI slot on the DRM-sysfs
/// path), so a second probe that only knows the PCI slot — the
/// [`crate::scanout`] KMS probe — cannot re-derive an identity that equals this
/// one. It instead **reuses this canonical `DeviceId`** for the same physical
/// GPU, matching on this PCI bus id. The field is deliberately **excluded from
/// equality and hash** so populating it never perturbs the placement key.
#[derive(Debug, Clone)]
pub struct DeviceId {
    vendor: Vendor,
    stable_id: String,
    index: u32,
    pci_bus_id: Option<String>,
}

impl DeviceId {
    /// Construct a device identity from its vendor, stable id, and enumeration
    /// index.
    ///
    /// `stable_id` must be the vendor's stable handle (UUID / PCI bus id /
    /// registryID). `index` is only ever a deterministic tie-breaker.
    #[must_use]
    pub fn new(vendor: Vendor, stable_id: impl Into<String>, index: u32) -> Self {
        Self {
            vendor,
            stable_id: stable_id.into(),
            index,
            pci_bus_id: None,
        }
    }

    /// Attach the device's PCI bus id (the non-identity cross-probe matching key).
    ///
    /// Set by every probe that knows it — the NVML load probe from
    /// `PciInfo.bus_id`, the DRM-sysfs probe from `PCI_SLOT_NAME` — so the
    /// [`crate::scanout`] KMS probe can reconcile a DRM card to this canonical
    /// `DeviceId`. It does **not** change `(vendor, stable_id)` identity.
    #[must_use]
    pub fn with_pci_bus_id(mut self, pci_bus_id: impl Into<String>) -> Self {
        self.pci_bus_id = Some(pci_bus_id.into());
        self
    }

    /// The device's vendor family.
    #[must_use]
    pub const fn vendor(&self) -> Vendor {
        self.vendor
    }

    /// The stable vendor handle (UUID / PCI bus id / registryID).
    #[must_use]
    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }

    /// The enumeration index — a deterministic tie-breaker only, never an
    /// identity.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// The raw PCI bus id this device was tagged with, if any (the non-identity
    /// cross-probe matching key, in its source form).
    #[must_use]
    pub fn pci_bus_id(&self) -> Option<&str> {
        self.pci_bus_id.as_deref()
    }

    /// The PCI bus id in canonical form ([`normalize_pci_bus_id`]) for a
    /// cross-source compare, if this device was tagged with one.
    ///
    /// Maps NVML's 8-hex-digit-domain `00000000:03:00.0` and the kernel
    /// 4-hex-digit-domain `0000:03:00.0` for the same slot onto one string, so a
    /// scanout probe's PCI slot and this device's PCI bus id compare equal for the
    /// same physical GPU.
    #[must_use]
    pub fn pci_bus_id_normalized(&self) -> Option<String> {
        self.pci_bus_id.as_deref().map(normalize_pci_bus_id)
    }
}

/// Normalise a PCI bus id for cross-source comparison (scanout ↔ render-node).
///
/// NVML reports `00000000:03:00.0` (8-hex-digit domain); the kernel sysfs
/// `PCI_SLOT_NAME` reports `0000:03:00.0` (4-hex-digit domain). Lower-casing and
/// reducing the domain segment to its trailing 4 hex digits (left-padding a
/// shorter domain with `0`) maps both onto the same canonical `0000:03:00.0`
/// form so a string compare is sound. A string with no `:` is returned
/// lower-cased unchanged (it still compares equal to itself). This mirrors the
/// `multiview-compositor` wgpu-adapter matcher's identical normalization.
#[must_use]
pub fn normalize_pci_bus_id(bus_id: &str) -> String {
    let lower = bus_id.trim().to_ascii_lowercase();
    match lower.split_once(':') {
        Some((domain, rest)) => {
            let tail: String = domain
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let padded = format!("{tail:0>4}");
            format!("{padded}:{rest}")
        }
        None => lower,
    }
}

impl PartialEq for DeviceId {
    /// Identity is `(vendor, stable_id)`; the enumeration index is deliberately
    /// excluded so a reordering across reboots does not change identity.
    fn eq(&self, other: &Self) -> bool {
        self.vendor == other.vendor && self.stable_id == other.stable_id
    }
}

impl Eq for DeviceId {}

impl core::hash::Hash for DeviceId {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.vendor.hash(state);
        self.stable_id.hash(state);
    }
}

/// A live per-device load snapshot (ADR-0017's `DeviceLoad`).
///
/// Produced by a [`LoadProbe`] (or injected in tests), consumed by the pure
/// selection policy in [`crate::select`] and mirrored into per-GPU Prometheus
/// gauges in `multiview-telemetry`.
///
/// Every load field is an [`Option`] because **availability is
/// vendor-asymmetric** (the honest matrix,
/// [gpu-monitoring §1](../../../docs/research/gpu-monitoring-and-scheduling.md)):
/// per-engine encoder/decoder utilisation is clean on NVIDIA + Intel,
/// per-process-only/merged on AMD, and absent on Apple. `None` means **unknown**
/// — the selector drops it from the score and redistributes weight; it is never
/// fabricated as `0.0`, and the telemetry layer does not register a gauge for
/// it (so dashboards show "n/a", not a false zero).
///
/// Fractions are normalised `0.0..=1.0` busy fractions; bytes are absolute.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DeviceLoad {
    /// The device this load describes (stable identity).
    pub device_id: DeviceId,
    /// GPU core / compute busy fraction (`0.0..=1.0`), where exposed. This is
    /// the SM/3D-engine busy fraction (compositor pressure), **not** the memory
    /// controller (the verified NVML `.memory` trap is avoided —
    /// [gpu-monitoring §1](../../../docs/research/gpu-monitoring-and-scheduling.md)).
    pub gpu_busy_frac: Option<f32>,
    /// VRAM bytes currently in use across all consumers, where exposed.
    pub vram_used_bytes: Option<u64>,
    /// Total VRAM bytes, where exposed. With `vram_used_bytes` this is the
    /// authoritative VRAM-pressure pair (`nvmlDeviceGetMemoryInfo`), never the
    /// memory-controller busy %.
    pub vram_total_bytes: Option<u64>,
    /// Encoder-ASIC busy fraction (`0.0..=1.0`), where the vendor meters the
    /// encode engine on its own counter. On AMD VCN4+ this is the *combined*
    /// media figure; on Apple it is always `None`.
    pub enc_util_frac: Option<f32>,
    /// Decoder-ASIC busy fraction (`0.0..=1.0`), where metered per-engine. On
    /// AMD VCN4+ this is the *combined* media figure; on Apple always `None`.
    pub dec_util_frac: Option<f32>,
    /// Concurrent NVENC encode-session count for this device, where the vendor
    /// exposes it (NVIDIA only). Feeds the per-system session-ceiling gate in
    /// [`crate::select`].
    pub nvenc_session_count: Option<u32>,
    /// Compute-engine busy fraction (`0.0..=1.0`) as distinct from
    /// `gpu_busy_frac` on vendors that separate a compute queue from the 3D
    /// queue; `None` collapses to `gpu_busy_frac` for the compositor-pressure
    /// term.
    pub compute_busy_frac: Option<f32>,
}

impl DeviceLoad {
    /// Construct an all-unknown load snapshot for `device_id` — every signal
    /// `None`.
    ///
    /// This is the honest starting point a blind vendor (Apple) or a
    /// probe-before-first-sample yields: the selector falls back to the cost
    /// model, and no telemetry gauge is registered for an unknown field.
    #[must_use]
    pub const fn unknown(device_id: DeviceId) -> Self {
        Self {
            device_id,
            gpu_busy_frac: None,
            vram_used_bytes: None,
            vram_total_bytes: None,
            enc_util_frac: None,
            dec_util_frac: None,
            nvenc_session_count: None,
            compute_busy_frac: None,
        }
    }

    /// Fraction of VRAM in use (`used / total`), if both bytes are known and
    /// `total > 0`.
    ///
    /// This is the **primary**, highest-weighted selection signal (a hard OOM
    /// wall) and the one signal trustworthy on every vendor.
    #[must_use]
    pub fn vram_used_frac(&self) -> Option<f32> {
        match (self.vram_used_bytes, self.vram_total_bytes) {
            (Some(used), Some(total)) if total > 0 => {
                // u32-domain fractions: widen both to f64 losslessly, divide,
                // then clamp to [0,1] and narrow to f32. No `as` casts.
                let frac = bytes_ratio(used, total);
                Some(frac)
            }
            _ => None,
        }
    }

    /// Free VRAM in bytes (`total - used`), if both are known. Saturates at `0`
    /// if a transient sample reports `used > total`.
    #[must_use]
    pub fn vram_free_bytes(&self) -> Option<u64> {
        match (self.vram_used_bytes, self.vram_total_bytes) {
            (Some(used), Some(total)) => Some(total.saturating_sub(used)),
            _ => None,
        }
    }

    /// The compositor-pressure busy fraction: `compute_busy_frac` if present,
    /// else `gpu_busy_frac`. `None` if neither is exposed.
    #[must_use]
    pub fn effective_compute_frac(&self) -> Option<f32> {
        self.compute_busy_frac.or(self.gpu_busy_frac)
    }
}

/// Our-process share of one device's load (monitoring v2: "ours vs total").
///
/// A [`DeviceLoad`] carries the **device-wide** totals across every consumer of a
/// shared GPU (e.g. a co-tenant NVR). A `SelfShare` carries the portion attributed
/// to **this** process (and its in-process libav NVENC/NVDEC contexts), keyed by
/// the same stable [`DeviceId`], so the UI can render "ours vs total". Every
/// signal is `Option`: present only where the platform exposes a per-process
/// counter (NVIDIA via NVML per-process queries) and we are actually resident;
/// `None` is the honest unknown, **never** a fabricated zero.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SelfShare {
    /// The device this share describes (same stable identity as the matching
    /// [`DeviceLoad`]).
    pub device_id: DeviceId,
    /// Our process's compute (SM) busy fraction (`0.0..=1.0`) on this device,
    /// where the platform exposes a per-process SM counter.
    pub compute_util: Option<f32>,
    /// Our process's encoder (NVENC) busy fraction (`0.0..=1.0`), where exposed.
    pub encoder_util: Option<f32>,
    /// Our process's decoder (NVDEC) busy fraction (`0.0..=1.0`), where exposed.
    pub decoder_util: Option<f32>,
    /// VRAM (bytes) attributed to our process on this device, summed across our
    /// graphics + compute contexts, where exposed.
    pub mem_used_bytes: Option<u64>,
    /// Concurrent NVENC encode sessions owned by our process on this device. A
    /// `Some(0)` is an honest "we queried and own none"; `None` is "not queried /
    /// unavailable".
    pub encoder_sessions: Option<u32>,
}

impl SelfShare {
    /// Construct an all-unknown self-share for `device_id` — every signal `None`.
    ///
    /// The honest starting point before any per-process counter is read (or on a
    /// platform that exposes none): the wire `self_*` fields then stay absent.
    #[must_use]
    pub const fn unknown(device_id: DeviceId) -> Self {
        Self {
            device_id,
            compute_util: None,
            encoder_util: None,
            decoder_util: None,
            mem_used_bytes: None,
            encoder_sessions: None,
        }
    }
}

/// Sum the VRAM bytes attributed to `our_pid` across a device's running-process
/// list, given as `(pid, used_bytes)` pairs (a `None` value is a driver
/// "unavailable" for that entry).
///
/// Returns `Some(total)` only when at least one entry is ours **and** carries a
/// known byte count; `None` when we are not resident on the device or the driver
/// reports our memory as unavailable — the honest unknown, never a false zero. A
/// process may appear more than once (a graphics + a compute context); those sum.
///
/// Pure + GPU-free so it is unit-tested in the default build; consumed by the
/// `cuda`-gated NVML per-process pass, hence gated to where it is referenced
/// (`cuda` or `test`) to stay dead-code-clean under `-D warnings`.
#[cfg(any(feature = "cuda", test))]
#[must_use]
fn self_mem_from_processes(processes: &[(u32, Option<u64>)], our_pid: u32) -> Option<u64> {
    let mut total: Option<u64> = None;
    for &(pid, used) in processes {
        if pid != our_pid {
            continue;
        }
        if let Some(bytes) = used {
            total = Some(total.unwrap_or(0).saturating_add(bytes));
        }
    }
    total
}

/// Count the encode sessions owned by `our_pid` from a device's session-owner pid
/// list.
///
/// We always queried (the caller only calls this when the session list was read),
/// so a count of `0` is an honest "we own none of the active sessions", distinct
/// from "not queried" (which the caller represents as `None`).
///
/// Pure + GPU-free (unit-tested in the default build); gated to where it is
/// referenced (`cuda` or `test`) to stay dead-code-clean under `-D warnings`.
#[cfg(any(feature = "cuda", test))]
#[must_use]
fn self_sessions_from(session_owner_pids: &[u32], our_pid: u32) -> u32 {
    let count = session_owner_pids
        .iter()
        .filter(|&&pid| pid == our_pid)
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Average the utilisation rows belonging to `our_pid`, given as `(pid, percent)`
/// pairs (NVML reports per-process SM/enc/dec utilisation as 0-100 integers), and
/// map the mean to a clamped `0.0..=1.0` fraction.
///
/// Returns `None` when no row is ours (idle, or the driver returned no
/// per-process sample for us) — the honest unknown, never a fabricated `0.0`.
///
/// Pure + GPU-free (unit-tested in the default build); gated to where it is
/// referenced (`cuda` or `test`) to stay dead-code-clean under `-D warnings`.
#[cfg(any(feature = "cuda", test))]
#[must_use]
fn self_util_avg_from_samples(samples: &[(u32, u32)], our_pid: u32) -> Option<f32> {
    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    for &(pid, percent) in samples {
        if pid != our_pid {
            continue;
        }
        sum = sum.saturating_add(u64::from(percent.min(100)));
        count = count.saturating_add(1);
    }
    if count == 0 {
        return None;
    }
    // mean percent in 0..=100; divide to a unit fraction via lossless widening.
    let mean = u64_to_f64(sum) / u64_to_f64(count);
    Some(f64_to_f32_saturating((mean / 100.0).clamp(0.0, 1.0)))
}

/// `used / total` as an `f32` fraction clamped to `0.0..=1.0`, computed via
/// lossless `u64 -> f64` widening (no `as` casts).
///
/// `total` is assumed `> 0` by the caller. Both byte counts are well below
/// `2^53` for real hardware, so the widening is exact.
fn bytes_ratio(used: u64, total: u64) -> f32 {
    let used = u64_to_f64(used);
    let total = u64_to_f64(total);
    let frac = (used / total).clamp(0.0, 1.0);
    f64_to_f32_saturating(frac)
}

/// `numerator / denominator` as an `f32` fraction clamped to `0.0..=1.0`, via
/// the same lossless `u64 -> f64` widening as [`bytes_ratio`] (no `as` casts).
///
/// Used to turn a busy-ns delta over a wall-ns interval into a busy fraction
/// (the DRM fdinfo media-engine term). `denominator` is assumed `> 0` by the
/// caller; a `0` numerator yields `0.0`.
#[cfg(any(feature = "vaapi", feature = "qsv"))]
fn busy_ratio(numerator: u64, denominator: u64) -> f32 {
    if denominator == 0 {
        return 0.0;
    }
    let numerator = u64_to_f64(numerator);
    let denominator = u64_to_f64(denominator);
    let frac = (numerator / denominator).clamp(0.0, 1.0);
    f64_to_f32_saturating(frac)
}

/// Convert a `0..=100` integer percentage to a clamped `0.0..=1.0` busy
/// fraction, via the lossless `u16 -> f32` path (no `as` casts).
///
/// Shared by the sysfs `gpu_busy_percent` parser; mirrors the NVML
/// `percent_to_frac` helper but takes the already-`u32` sysfs value.
#[cfg(any(feature = "vaapi", feature = "qsv"))]
fn percent_to_busy_frac(percent: u32) -> f32 {
    let clamped = percent.min(100);
    let hundredths = u16::try_from(clamped).map_or(100.0_f32, f32::from);
    hundredths / 100.0_f32
}

/// Lossless `u64 -> f64` widening for byte counts (`< 2^53`), avoiding `as`.
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

/// Narrow an `f64` in `0.0..=1.0` to `f32` without an `as` cast.
///
/// The value is already clamped to the unit interval by the caller, so we map
/// it onto an exact integer grid (`2^24` ticks — `f32`'s integer-exactness
/// bound), recover that grid count through `TryFrom`/`f32::from` (no `as`), and
/// divide back. The result carries full f32 precision for a unit fraction.
fn f64_to_f32_saturating(value: f64) -> f32 {
    const SCALE: f64 = 16_777_216.0; // 2^24, exact in both f64 and f32.
    let scaled = (value * SCALE).round();
    if scaled <= 0.0 {
        return 0.0;
    }
    if scaled >= SCALE {
        return 1.0;
    }
    // 0 < scaled < 2^24 and integer-valued (`.round()`), so the f64 -> u32
    // conversion is lossless and in range via the string-free integer path.
    let ticks = u32::try_from(f64_trunc_to_u64(scaled)).unwrap_or(0);
    f32_from_u32(ticks) / 16_777_216.0_f32
}

/// Truncate a finite, non-negative `f64` to its integer part as a `u64`,
/// avoiding an `as` cast.
///
/// Implemented by reading the IEEE-754 fields: this is exact for any value
/// representable as an integer below `2^53` (our domain is `< 2^24`).
fn f64_trunc_to_u64(value: f64) -> u64 {
    let truncated = value.trunc();
    if truncated <= 0.0 {
        return 0;
    }
    let bits = truncated.to_bits();
    let exponent_biased = (bits >> 52) & 0x7FF;
    let mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
    // Subnormals and values < 1.0 truncate to 0 (already handled above for the
    // <= 0 case; a positive value < 1.0 has exponent < 1023).
    let Some(exponent) = exponent_biased.checked_sub(1023) else {
        return 0;
    };
    let significand = mantissa | 0x0010_0000_0000_0000; // implicit leading 1
                                                        // value = significand * 2^(exponent - 52)
    if exponent >= 52 {
        significand << (exponent - 52)
    } else {
        significand >> (52 - exponent)
    }
}

/// Exact `u32 -> f32` for values `<= 2^24` (the unit-fraction grid). Avoids an
/// `as` cast by composing two `u16` halves, each lossless via `f32::from`.
fn f32_from_u32(value: u32) -> f32 {
    let high = u16::try_from((value >> 16) & 0xFFFF).map_or(f32::INFINITY, f32::from);
    let low = u16::try_from(value & 0xFFFF).map_or(f32::INFINITY, f32::from);
    high * 65_536.0_f32 + low
}

/// The outcome of one probe sample for a single device.
///
/// Mirrors [`crate::probe::ProbeOutcome`]: a probe either returns a fresh
/// [`DeviceLoad`] or reports that the device/vendor is *unavailable* (feature
/// off, no device, or the vendor library failed to initialise) — cleanly, never
/// a panic.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum LoadSample {
    /// A fresh per-device load snapshot.
    Ready(DeviceLoad),
    /// No usable device / vendor library for this probe on this host.
    Unavailable {
        /// Static, human-readable reason (e.g. `"NVML library not present"`).
        reason: &'static str,
    },
}

impl LoadSample {
    /// The [`DeviceLoad`] if this sample is [`LoadSample::Ready`], else `None`.
    #[must_use]
    pub fn load(&self) -> Option<&DeviceLoad> {
        match self {
            LoadSample::Ready(load) => Some(load),
            LoadSample::Unavailable { .. } => None,
        }
    }

    /// Whether this sample carries a usable load snapshot.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, LoadSample::Ready(_))
    }
}

/// The vendor seam for live-load probing.
///
/// A `LoadProbe` enumerates the devices it can see and samples each one's
/// current load. It mirrors the injectable [`crate::probe::DeviceProbe`]: real
/// feature-gated impls (NVML, i915, amdgpu) implement it, and tests inject a
/// double. Implementations must be **non-blocking-on-the-engine** and bounded:
/// the [`LoadPoller`] runs them off the hot path (ADR-0017 §2).
///
/// Probing must never panic. A vendor library that fails to initialise, or a
/// host with no device, yields [`LoadSample::Unavailable`] (and, for `sample`,
/// an empty/`Unavailable` result) — exactly the graceful-absence contract.
pub trait LoadProbe {
    /// The stable identities of the devices this probe can currently see.
    ///
    /// Returns an empty vector cleanly when the vendor library is absent or no
    /// device is present.
    fn devices(&self) -> Vec<DeviceId>;

    /// Sample the current load of one device.
    ///
    /// Returns [`LoadSample::Unavailable`] (never a panic) when the device is
    /// gone or the vendor library cannot answer.
    fn sample(&self, device: &DeviceId) -> LoadSample;

    /// Sample every visible device in one pass.
    ///
    /// The default walks [`LoadProbe::devices`] and [`LoadProbe::sample`]; a
    /// vendor impl may override for a cheaper single-pass query. Only
    /// [`LoadSample::Ready`] samples are returned.
    fn sample_all(&self) -> Vec<DeviceLoad> {
        self.devices()
            .iter()
            .filter_map(|d| match self.sample(d) {
                LoadSample::Ready(load) => Some(load),
                LoadSample::Unavailable { .. } => None,
            })
            .collect()
    }
}

/// A bounded, off-hot-path poller around a [`LoadProbe`].
///
/// This is the runtime abstraction ADR-0017 §2 describes: the engine owns a
/// dedicated blocking thread that drives `poll()` at ~1-4 Hz and publishes the
/// resulting `Vec<DeviceLoad>` into a wait-free snapshot (`arc_swap`, owned by
/// `multiview-engine`) plus the telemetry gauges. The poller itself holds only the
/// probe and the configured cadence — it does **no** I/O, spawns **no** thread,
/// and never blocks the engine; the engine decides when to call `poll`. Keeping
/// it this thin makes it pure and unit-testable with an injected probe, and
/// keeps invariants #1/#10 the caller's structural guarantee.
#[derive(Debug, Clone)]
pub struct LoadPoller<P: LoadProbe> {
    probe: P,
    interval: PollInterval,
}

impl<P: LoadProbe> LoadPoller<P> {
    /// Wrap a probe with a polling cadence.
    #[must_use]
    pub const fn new(probe: P, interval: PollInterval) -> Self {
        Self { probe, interval }
    }

    /// The configured poll cadence.
    #[must_use]
    pub const fn interval(&self) -> PollInterval {
        self.interval
    }

    /// Borrow the underlying probe.
    #[must_use]
    pub const fn probe(&self) -> &P {
        &self.probe
    }

    /// Take one sample pass over every visible device.
    ///
    /// This is what the engine's blocking poll thread calls each tick. Returns
    /// the fresh loads to publish into the wait-free snapshot + the gauges.
    #[must_use]
    pub fn poll(&self) -> Vec<DeviceLoad> {
        self.probe.sample_all()
    }
}

/// The hardware-addressing handles needed to **pin a chosen device** at every
/// pipeline stage (ADR-0035 Tier-1 / the GPU-placement principle).
///
/// A [`DeviceLoad`]/[`DeviceId`] identifies a device by its *stable* handle (NVML
/// UUID) — perfect for placement + pins, but **not** the key the downstream
/// hardware APIs address a GPU by. The wgpu compositor matches on the **PCI bus
/// id** (or the `(vendor, device)` PCI pair / name); libav NVDEC/NVENC address by
/// the CUDA enumeration **ordinal**. This is the small bag of those handles for a
/// single device, resolved off-hot-path at admission so the one chosen device
/// reaches wgpu, NVDEC, and NVENC as a single value (affinity).
///
/// Every field is `Option`: a vendor/platform that does not expose a handle
/// yields `None` (the honest unknown), and the consumer falls back to a coarser
/// discriminator or its default path — never a fabricated value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GpuTargetInfo {
    /// The PCI bus id (NVML `PciInfo.bus_id`, e.g. `00000000:01:00.0`) — the
    /// robust cross-API key the wgpu compositor matches its adapter on.
    pub pci_bus_id: Option<String>,
    /// The PCI vendor id (e.g. `0x10de` for NVIDIA), the fallback discriminator.
    pub vendor_id: Option<u32>,
    /// The PCI device id, paired with `vendor_id` as the fallback discriminator.
    pub device_id: Option<u32>,
    /// The human-readable adapter name (e.g. `NVIDIA GeForce RTX 4060`).
    pub name: Option<String>,
    /// The CUDA enumeration **ordinal** as a string (e.g. `"1"`) — the selector
    /// libav's `cuda` hwdevice / `*_cuvid` decoders + `*_nvenc` encoders address a
    /// GPU by. This is the Tier-2 NVDEC/NVENC pin value (the hardware decode/encode
    /// paths consume it once they are wired into the run).
    pub cuda_ordinal: Option<String>,
}

/// An object-safe "where the live load comes from" seam.
///
/// [`LoadPoller`] is generic over its [`LoadProbe`], which is convenient at the
/// engine seam but cannot be stored behind a `dyn` pointer. A consumer that
/// merely needs *a* source of [`DeviceLoad`] snapshots at runtime — the
/// `multiview-cli` system-metrics task, which selects an NVML-backed poller when
/// the `cuda` feature is on and the always-compiled [`NullLoadPoller`] otherwise
/// — injects a `Box<dyn LoadSource>` and calls [`LoadSource::poll`] once per
/// metrics tick. The same off-hot-path / non-blocking contract as
/// [`LoadPoller::poll`] applies: a `poll` does a bounded vendor query and never
/// blocks the engine (it runs on the metrics task, not the output clock).
pub trait LoadSource {
    /// Take one snapshot pass over every visible device.
    ///
    /// Returns an empty vector cleanly when no accelerator is visible (the
    /// no-GPU host) — never a panic, never a fabricated device.
    fn poll(&self) -> Vec<DeviceLoad>;

    /// Take one **our-process share** pass over every visible device (monitoring
    /// v2: "ours vs total").
    ///
    /// [`poll`](Self::poll) reports the device-wide totals across every consumer of
    /// a shared GPU; this reports the portion attributed to *this* process via the
    /// platform's per-process counters (NVIDIA NVML), keyed by the same stable
    /// [`DeviceId`]. The default returns an empty vector — a source with no
    /// per-process signal (the [`NullLoadPoller`], a vendor that exposes none)
    /// honestly attributes nothing, never a fabricated zero. The same off-hot-path
    /// / non-blocking contract as [`poll`](Self::poll) applies.
    fn poll_self_share(&self) -> Vec<SelfShare> {
        Vec::new()
    }

    /// Resolve the hardware-addressing handles ([`GpuTargetInfo`]) for `device`,
    /// so a chosen device can be **pinned** at every pipeline stage (wgpu / NVDEC
    /// / NVENC) — the ADR-0035 Tier-1 affinity seam.
    ///
    /// The default returns `None`: a source with no per-device hardware-handle
    /// signal (the [`NullLoadPoller`], a vendor that exposes none) cannot pin a
    /// device, so the consumer keeps its default adapter path. The NVIDIA NVML
    /// source overrides this to return the device's PCI bus id, `(vendor,
    /// device)` pair, name, and CUDA ordinal. Off-hot-path / non-blocking, exactly
    /// like [`poll`](Self::poll).
    fn device_target(&self, device: &DeviceId) -> Option<GpuTargetInfo> {
        let _ = device;
        None
    }

    /// Read each visible device's **static perf signals** (name, shader-core
    /// count, max graphics clock, architecture) — the priors the perf-class model
    /// ([`crate::perf::PerfClass::for_device`]) turns into a real per-GPU
    /// [`crate::cost::CostBudget`].
    ///
    /// These are immutable hardware facts, not live load, so a consumer reads them
    /// **once** at pipeline-build time (off the output-clock thread) and keys them
    /// by the same stable [`DeviceId`] as [`poll`](Self::poll). The default returns
    /// an empty vector — a source with no perf signal (the [`NullLoadPoller`], a
    /// vendor that exposes none) supplies nothing, and the candidate falls back to
    /// [`crate::perf::DEFAULT_PERF_CLASS`]; never a fabricated value. The same
    /// off-hot-path / non-blocking contract as [`poll`](Self::poll) applies.
    fn device_perf(&self) -> Vec<(DeviceId, crate::perf::PerfSignals)> {
        Vec::new()
    }
}

impl<P: LoadProbe> LoadSource for LoadPoller<P> {
    fn poll(&self) -> Vec<DeviceLoad> {
        LoadPoller::poll(self)
    }
}

/// The always-compiled no-GPU load source: every [`NullLoadPoller::poll`] yields
/// zero devices.
///
/// This is the honest default a host with no accelerator (or a pure-Rust build
/// with every vendor feature off) uses, so the `multiview-cli` system-metrics
/// task always has a working [`LoadSource`] to inject — it simply publishes a
/// `SystemMetrics` with an empty `gpus` list, never a fabricated GPU. It does no
/// I/O, holds no state, and cannot fail.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullLoadPoller;

impl NullLoadPoller {
    /// Construct the no-GPU load source.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl LoadSource for NullLoadPoller {
    fn poll(&self) -> Vec<DeviceLoad> {
        Vec::new()
    }
}

/// A bounded poll cadence, in hertz, clamped to the ADR-0017 ~1-4 Hz envelope.
///
/// A newtype so the cadence cannot be set to an unbounded value that would turn
/// the probe into a hot-path cost (the fdinfo walk on tiny boxes; §5 risk 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollInterval {
    millis: u32,
}

impl PollInterval {
    /// Minimum poll period (250 ms = 4 Hz, the fast end of the envelope).
    pub const MIN_MILLIS: u32 = 250;
    /// Maximum poll period (1000 ms = 1 Hz, the slow end of the envelope).
    pub const MAX_MILLIS: u32 = 1000;

    /// Construct a poll interval from a period in milliseconds, clamped to the
    /// `MIN_MILLIS..=MAX_MILLIS` envelope.
    #[must_use]
    pub const fn from_millis(millis: u32) -> Self {
        let clamped = if millis < Self::MIN_MILLIS {
            Self::MIN_MILLIS
        } else if millis > Self::MAX_MILLIS {
            Self::MAX_MILLIS
        } else {
            millis
        };
        Self { millis: clamped }
    }

    /// The (clamped) poll period in milliseconds.
    #[must_use]
    pub const fn as_millis(self) -> u32 {
        self.millis
    }
}

impl Default for PollInterval {
    /// 2 Hz (500 ms) — the middle of the ADR-0017 envelope.
    fn default() -> Self {
        Self::from_millis(500)
    }
}

// ----------------------------------------------------------------------------
// Feature-gated vendor probes.
//
// Each probe has a feature-off arm (absent from the build) and a feature-on arm
// that performs the real query. The feature-on arm MUST report Unavailable
// cleanly when the vendor library fails to initialise or no device is present
// (CI / this box) — never a panic. The NVIDIA path uses the runtime-loaded
// `nvml-wrapper` (libloading) so there is no build-time native dep.
// ----------------------------------------------------------------------------

#[cfg(feature = "cuda")]
pub use self::nvml::{NvmlLoadPoller, NvmlLoadProbe};

#[cfg(feature = "cuda")]
mod nvml {
    use super::{
        self_mem_from_processes, self_sessions_from, self_util_avg_from_samples, DeviceId,
        DeviceLoad, GpuTargetInfo, LoadPoller, LoadProbe, LoadSample, LoadSource, PollInterval,
        SelfShare, Vendor,
    };
    use nvml_wrapper::enums::device::UsedGpuMemory;

    /// NVIDIA live-load probe via NVML (`nvml-wrapper`, runtime-loaded through
    /// `libloading`).
    ///
    /// Construction is fallible-but-graceful: [`NvmlLoadProbe::try_init`]
    /// returns `None` when NVML is not present (no driver, no device, library
    /// not loadable) — exactly the no-GPU fallback. Once initialised, every
    /// query that the device cannot answer maps to an unknown field (`None`) or
    /// [`LoadSample::Unavailable`]; nothing here panics.
    pub struct NvmlLoadProbe {
        nvml: nvml_wrapper::Nvml,
    }

    impl core::fmt::Debug for NvmlLoadProbe {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("NvmlLoadProbe").finish_non_exhaustive()
        }
    }

    impl NvmlLoadProbe {
        /// Try to initialise NVML, returning `None` if it is unavailable.
        ///
        /// On a host with no NVIDIA driver/device (this box, CI) `Nvml::init`
        /// returns an error which we map to `None` — initialise-or-unavailable,
        /// never a panic (ADR-0017 §2 design rule 1).
        #[must_use]
        pub fn try_init() -> Option<Self> {
            match nvml_wrapper::Nvml::init() {
                Ok(nvml) => Some(Self { nvml }),
                Err(error) => {
                    tracing::debug!(%error, "NVML unavailable; GPU load probe disabled");
                    None
                }
            }
        }

        /// Compute **our process's** share of one device's load (monitoring v2).
        ///
        /// Attributes to `our_pid` (this Multiview process — it owns the in-process
        /// libav NVENC/NVDEC sessions) via NVML's per-process queries:
        /// * VRAM: `running_graphics_processes` + `running_compute_processes`
        ///   (`ProcessInfo.used_gpu_memory`), summed across our contexts.
        /// * Encode sessions: `encoder_sessions` filtered to our pid (count).
        /// * SM/enc/dec utilisation: `process_utilization_stats` rows for our pid,
        ///   averaged (a sampling window the driver may not always populate — then
        ///   `None`, never a false zero).
        ///
        /// Returns `None` only when the device handle is unavailable or its identity
        /// changed under us; otherwise a `SelfShare` whose individual fields are
        /// `None` exactly where NVML did not expose that per-process signal.
        #[must_use]
        pub fn self_share(&self, device: &DeviceId, our_pid: u32) -> Option<SelfShare> {
            let handle = self.nvml.device_by_index(device.index()).ok()?;
            // Guard against a handle reordering: if the UUID changed, don't mislabel.
            if handle.uuid().map_or(true, |u| u != device.stable_id()) {
                return None;
            }

            // VRAM attributed to us: sum our graphics + compute contexts. A driver
            // that reports memory as `Unavailable` for an entry contributes `None`
            // for that entry (collapsed by the pure summer to "unknown", not zero).
            let mut mem_pairs: Vec<(u32, Option<u64>)> = Vec::new();
            let mut saw_process_list = false;
            if let Ok(graphics) = handle.running_graphics_processes() {
                saw_process_list = true;
                mem_pairs.extend(
                    graphics
                        .iter()
                        .map(|p| (p.pid, used_bytes(&p.used_gpu_memory))),
                );
            }
            if let Ok(compute) = handle.running_compute_processes() {
                saw_process_list = true;
                mem_pairs.extend(
                    compute
                        .iter()
                        .map(|p| (p.pid, used_bytes(&p.used_gpu_memory))),
                );
            }
            let mem_used_bytes = if saw_process_list {
                self_mem_from_processes(&mem_pairs, our_pid)
            } else {
                None
            };

            // Encode sessions we own: a count (Some(0) is "we own none", distinct
            // from None = "the session list was unavailable").
            let encoder_sessions = handle.encoder_sessions().ok().map(|sessions| {
                let owners: Vec<u32> = sessions.into_iter().map(|s| s.pid).collect();
                self_sessions_from(&owners, our_pid)
            });

            // Per-process SM/enc/dec utilisation: the most recent sampling window.
            // Passing 0 as last_seen_timestamp asks for all available samples; the
            // driver may return none (no recent window) -> each util stays None.
            let (compute_util, encoder_util, decoder_util) = match handle
                .process_utilization_stats(None)
            {
                Ok(samples) => {
                    let sm: Vec<(u32, u32)> = samples.iter().map(|s| (s.pid, s.sm_util)).collect();
                    let enc: Vec<(u32, u32)> =
                        samples.iter().map(|s| (s.pid, s.enc_util)).collect();
                    let dec: Vec<(u32, u32)> =
                        samples.iter().map(|s| (s.pid, s.dec_util)).collect();
                    (
                        self_util_avg_from_samples(&sm, our_pid),
                        self_util_avg_from_samples(&enc, our_pid),
                        self_util_avg_from_samples(&dec, our_pid),
                    )
                }
                Err(_) => (None, None, None),
            };

            Some(SelfShare {
                device_id: device.clone(),
                compute_util,
                encoder_util,
                decoder_util,
                mem_used_bytes,
                encoder_sessions,
            })
        }

        /// Resolve the hardware-addressing handles ([`GpuTargetInfo`]) for
        /// `device`: its PCI bus id, `(vendor, device)` PCI pair, name, and CUDA
        /// ordinal — so a chosen NVIDIA device can be pinned at wgpu / NVDEC /
        /// NVENC (ADR-0035 Tier-1 affinity).
        ///
        /// Returns `None` only when the device handle is unavailable or its
        /// identity changed under us (a handle reordering); otherwise a
        /// `GpuTargetInfo` whose individual fields are `None` exactly where NVML
        /// did not expose that handle — never a fabricated value. The CUDA ordinal
        /// is the device's NVML enumeration index (the same ordinal libav's `cuda`
        /// hwdevice addresses), as a string.
        #[must_use]
        pub fn device_target(&self, device: &DeviceId) -> Option<GpuTargetInfo> {
            let handle = self.nvml.device_by_index(device.index()).ok()?;
            // Guard against a handle reordering: if the UUID changed, don't pin
            // the wrong card.
            if handle.uuid().map_or(true, |u| u != device.stable_id()) {
                return None;
            }
            let pci_bus_id = handle.pci_info().ok().map(|info| info.bus_id);
            let name = handle.name().ok();
            Some(GpuTargetInfo {
                pci_bus_id,
                // NVML reports the combined `pciDeviceId` (device<<16 | vendor);
                // split it into the wgpu-comparable 16-bit vendor + device ids.
                vendor_id: handle
                    .pci_info()
                    .ok()
                    .map(|info| info.pci_device_id & 0xFFFF),
                device_id: handle
                    .pci_info()
                    .ok()
                    .map(|info| (info.pci_device_id >> 16) & 0xFFFF),
                name,
                cuda_ordinal: Some(device.index().to_string()),
            })
        }

        /// Read one device's **static perf signals** for the perf-class model.
        ///
        /// Populates [`crate::perf::PerfSignals`] from NVML's immutable hardware
        /// queries: `name()`, `num_cores()` (the CUDA-core count), the max
        /// graphics-domain clock (`max_clock_info(Clock::Graphics)`, MHz), and the
        /// `architecture()` generation (stringified via its `Display`, e.g.
        /// `"Ada"`/`"Pascal"`). Each query is independently optional: a device or
        /// driver that cannot answer one leaves that field `None` (the honest
        /// unknown — the perf-class resolver simply drops to the next signal),
        /// never a fabricated value and never a panic.
        ///
        /// Returns `None` only when the device handle is unavailable or its
        /// identity changed under us (a reordering guard, matching
        /// [`Self::self_share`]).
        #[must_use]
        pub fn device_perf(&self, device: &DeviceId) -> Option<crate::perf::PerfSignals> {
            let handle = self.nvml.device_by_index(device.index()).ok()?;
            // Guard against a handle reordering: never mislabel a different card.
            if handle.uuid().map_or(true, |u| u != device.stable_id()) {
                return None;
            }
            Some(crate::perf::PerfSignals {
                name: handle.name().ok(),
                num_cores: handle.num_cores().ok(),
                max_graphics_clock_mhz: handle
                    .max_clock_info(nvml_wrapper::enum_wrappers::device::Clock::Graphics)
                    .ok(),
                architecture: handle.architecture().ok().map(|arch| arch.to_string()),
            })
        }
    }

    /// Extract the byte count from an NVML [`UsedGpuMemory`], mapping the WDDM /
    /// not-available variant to `None` (the honest unknown, never a false zero).
    fn used_bytes(used: &UsedGpuMemory) -> Option<u64> {
        match *used {
            UsedGpuMemory::Used(bytes) => Some(bytes),
            UsedGpuMemory::Unavailable => None,
        }
    }

    impl LoadProbe for NvmlLoadProbe {
        fn devices(&self) -> Vec<DeviceId> {
            let Ok(count) = self.nvml.device_count() else {
                return Vec::new();
            };
            (0..count)
                .filter_map(|index| {
                    let device = self.nvml.device_by_index(index).ok()?;
                    // Stable identity: the NVML UUID, never the enumeration
                    // index (ADR-0017 §2.1). Also tag the PCI bus id (the
                    // non-identity cross-probe key) so the scanout KMS probe can
                    // reconcile a DRM card to THIS canonical DeviceId — the UUID
                    // is unknowable from `/sys/class/drm`, the PCI slot is the
                    // only handle both probes share.
                    let uuid = device.uuid().ok()?;
                    let mut id = DeviceId::new(Vendor::Nvidia, uuid, index);
                    if let Ok(info) = device.pci_info() {
                        id = id.with_pci_bus_id(info.bus_id);
                    }
                    Some(id)
                })
                .collect()
        }

        fn sample(&self, device: &DeviceId) -> LoadSample {
            let Ok(handle) = self.nvml.device_by_index(device.index()) else {
                return LoadSample::Unavailable {
                    reason: "NVML device handle unavailable",
                };
            };
            // Confirm identity still matches (handles reorderings): if the UUID
            // changed under us, report Unavailable rather than mislabel.
            if handle.uuid().map_or(true, |u| u != device.stable_id()) {
                return LoadSample::Unavailable {
                    reason: "NVML device identity changed",
                };
            }

            // Each query is independently optional: the authoritative VRAM
            // pressure comes from MemoryInfo (bytes), NOT UtilizationRates.memory
            // (the verified memory-controller-busy trap).
            let mem = handle.memory_info().ok();
            let util = handle.utilization_rates().ok();
            let enc = handle.encoder_utilization().ok();
            let dec = handle.decoder_utilization().ok();
            let sessions = handle.encoder_stats().ok().map(|stats| stats.session_count);

            LoadSample::Ready(DeviceLoad {
                device_id: device.clone(),
                gpu_busy_frac: util.map(|u| percent_to_frac(u.gpu)),
                vram_used_bytes: mem.as_ref().map(|m| m.used),
                vram_total_bytes: mem.as_ref().map(|m| m.total),
                enc_util_frac: enc.map(|u| percent_to_frac(u.utilization)),
                dec_util_frac: dec.map(|u| percent_to_frac(u.utilization)),
                nvenc_session_count: sessions,
                // NVML does not separate a compute queue; the compositor-pressure
                // term falls back to gpu_busy_frac via effective_compute_frac.
                compute_busy_frac: None,
            })
        }
    }

    /// Convert an NVML 0-100 integer percentage to a clamped `0.0..=1.0` f32.
    fn percent_to_frac(percent: u32) -> f32 {
        let clamped = percent.min(100);
        let hundredths = u16::try_from(clamped).map_or(100.0_f32, f32::from);
        hundredths / 100.0_f32
    }

    /// The concrete NVIDIA live-load poller: an [`NvmlLoadProbe`] wrapped in the
    /// bounded off-hot-path [`LoadPoller`] envelope, exposed as the object-safe
    /// [`LoadSource`] the `multiview-cli` system-metrics task injects.
    ///
    /// Construction is fallible-but-graceful ([`NvmlLoadPoller::try_init`]): it
    /// returns `None` when NVML is unavailable (no driver, no device, library not
    /// loadable), so the caller cleanly falls back to the always-compiled
    /// [`super::NullLoadPoller`]. Once initialised, [`LoadSource::poll`] performs
    /// one bounded NVML pass per metrics tick; every per-device query the hardware
    /// cannot answer maps to an unknown field (`None`), never a fabricated zero
    /// and never a panic.
    #[derive(Debug)]
    pub struct NvmlLoadPoller {
        inner: LoadPoller<NvmlLoadProbe>,
    }

    impl NvmlLoadPoller {
        /// Try to initialise NVML and wrap it in a poller at `interval`, returning
        /// `None` when NVML is unavailable on this host (no driver/device).
        ///
        /// The interval is clamped into the ADR-0017 ~1-4 Hz envelope by
        /// [`PollInterval`]; the system-metrics task drives `poll` at its own
        /// (slower) cadence, so this only bounds the per-pass cost.
        #[must_use]
        pub fn try_init(interval: PollInterval) -> Option<Self> {
            let probe = NvmlLoadProbe::try_init()?;
            Some(Self {
                inner: LoadPoller::new(probe, interval),
            })
        }
    }

    impl LoadSource for NvmlLoadPoller {
        fn poll(&self) -> Vec<DeviceLoad> {
            self.inner.poll()
        }

        fn poll_self_share(&self) -> Vec<SelfShare> {
            // Our pid owns the in-process libav NVENC/NVDEC contexts; attribute the
            // per-process counters to it. One bounded pass over every visible
            // device, the same off-hot-path contract as `poll`.
            let our_pid = std::process::id();
            let probe = self.inner.probe();
            probe
                .devices()
                .iter()
                .filter_map(|device| probe.self_share(device, our_pid))
                .collect()
        }

        fn device_target(&self, device: &DeviceId) -> Option<GpuTargetInfo> {
            // One bounded NVML pass to resolve the device's hardware-addressing
            // handles (PCI bus id / pair / name / ordinal) for pinning. Off
            // hot-path, never blocks the engine.
            self.inner.probe().device_target(device)
        }

        fn device_perf(&self) -> Vec<(DeviceId, super::super::perf::PerfSignals)> {
            // One bounded NVML pass reading each device's immutable perf priors,
            // keyed by stable identity. Off-hot-path, never blocks the engine.
            let probe = self.inner.probe();
            probe
                .devices()
                .into_iter()
                .filter_map(|device| probe.device_perf(&device).map(|perf| (device, perf)))
                .collect()
        }
    }
}

#[cfg(any(feature = "vaapi", feature = "qsv"))]
pub use self::linux_sysfs::{
    parse_fdinfo_merged_media_frac, parse_fdinfo_pdev, FdinfoMediaSnapshot, FdinfoMediaTracker,
    FdinfoSample, SysfsLoadPoller, SysfsLoadProbe,
};

#[cfg(any(feature = "vaapi", feature = "qsv"))]
mod linux_sysfs {
    use super::{
        percent_to_busy_frac, DeviceId, DeviceLoad, LoadProbe, LoadSample, LoadSource, Vendor,
    };
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Instant;

    /// The canonical Linux DRM sysfs root the probe walks (`/sys/class/drm`),
    /// where each physical GPU appears as a `cardN/device/` directory exposing
    /// the `vendor`/`device` PCI ids, `gpu_busy_percent`, and the
    /// `mem_info_vram_{total,used}` byte counters (ADR-0017; amdgpu sysfs [A1]).
    #[cfg(target_os = "linux")]
    const DRM_ROOT: &str = "/sys/class/drm";

    /// PCI vendor id for NVIDIA silicon (`/sys/.../device/vendor` = `0x10de`).
    const PCI_VENDOR_NVIDIA: u16 = 0x10de;
    /// PCI vendor id for AMD silicon (`0x1002`).
    const PCI_VENDOR_AMD: u16 = 0x1002;
    /// PCI vendor id for Intel silicon (`0x8086`).
    const PCI_VENDOR_INTEL: u16 = 0x8086;

    /// The canonical Linux proc root the fdinfo walk reads
    /// (`/proc/<pid>/fdinfo/<fd>`), where each process exposes its open DRM
    /// client fds with the per-engine `drm-engine-*` busy-ns counters.
    #[cfg(target_os = "linux")]
    const PROC_ROOT: &str = "/proc";

    /// The Linux Intel/AMD live-load probe over the DRM sysfs tree (ADR-0017).
    ///
    /// Construction binds a [`DrmRoot`] (real `/sys/class/drm` in production, a
    /// synthetic temp tree in tests) plus a proc root for the fdinfo walk (real
    /// `/proc` in production, a synthetic tree in tests). Enumeration classifies
    /// each `cardN` by its PCI vendor id and keys it by the **stable PCI bus id**
    /// (never the enumeration index, [gpu-monitoring §2.1]); sampling reads the
    /// SMU-aggregate `gpu_busy_percent` and the `mem_info_vram_*` byte counters
    /// via plain `std::fs` (no native lib, no `unsafe`).
    ///
    /// Per-engine encoder/decoder utilisation is not in sysfs — on AMD it is
    /// per-process DRM fdinfo only (merged decode+encode from VCN4) and on Intel
    /// it is the i915 PMU. [`SysfsLoadProbe::sample_all_with_media`] populates the
    /// merged-media term into `enc_util_frac`/`dec_util_frac` by walking the
    /// caller's own PIDs' `/proc/<pid>/fdinfo` and differencing two snapshots a
    /// known interval apart (the [`FdinfoMediaTracker`]); the plain `sample` path
    /// leaves them `None` (honest unknown). The Intel i915 **PMU**
    /// (`perf_event_open`, which needs `unsafe`) is a deliberate follow-on and is
    /// **not** wired here.
    ///
    /// Every read that fails (file absent, off-Linux, garbled) degrades to an
    /// unknown field or an empty enumeration — never a panic and never a block.
    #[derive(Debug, Clone)]
    pub struct SysfsLoadProbe {
        root: DrmRoot,
        proc_root: PathBuf,
        media_tracker: FdinfoMediaTracker,
        /// The monotonic clock read to timestamp each fdinfo snapshot at its
        /// walk (F1). Always [`SnapshotClock::Monotonic`] in production; a unit
        /// test injects a controllable clock to drive an exact
        /// snapshot-to-snapshot interval through the real poll seam.
        clock: SnapshotClock,
    }

    /// The monotonic clock the media pass reads to timestamp each fdinfo
    /// snapshot at its walk — the F1 divisor is the elapsed between the two
    /// SNAPSHOT reads, so the timestamp is taken *after* the walk.
    ///
    /// Production always reads the real monotonic clock (`Instant::now()`); a
    /// unit test injects a controllable clock so the snapshot-to-snapshot
    /// interval — and thus the exact merged-media fraction — is deterministic
    /// when driven through the real `sample_all_with_media` / `LoadSource::poll`
    /// seam, rather than relying on a saturating counter delta.
    #[derive(Debug, Clone, Default)]
    pub(crate) enum SnapshotClock {
        /// The real monotonic clock, used in production.
        #[default]
        Monotonic,
        /// A test clock whose value each read takes from shared state the test
        /// advances between polls.
        #[cfg(test)]
        Manual(std::sync::Arc<std::sync::Mutex<Instant>>),
    }

    impl SnapshotClock {
        /// Read the current instant — the real monotonic clock in production,
        /// the injected value under test. A poisoned test clock degrades to the
        /// real clock (never panics; a test never leaves it poisoned).
        fn now(&self) -> Instant {
            match self {
                SnapshotClock::Monotonic => Instant::now(),
                #[cfg(test)]
                SnapshotClock::Manual(at) => {
                    at.lock().map_or_else(|_| Instant::now(), |guard| *guard)
                }
            }
        }
    }

    impl Default for SysfsLoadProbe {
        fn default() -> Self {
            Self::new()
        }
    }

    impl SysfsLoadProbe {
        /// Construct the probe bound to the real `/sys/class/drm` + `/proc` roots.
        ///
        /// Both trees are Linux-only, so off-Linux the probe binds to paths that
        /// do not exist; enumeration then yields nothing and the fdinfo walk an
        /// empty snapshot (graceful absence, never a panic) while the parsing core
        /// stays available for tests on any OS.
        #[must_use]
        pub fn new() -> Self {
            // Bind the real roots only on Linux; elsewhere there is no DRM sysfs
            // and no /proc fdinfo, so non-existent paths make the walks return
            // empty.
            #[cfg(target_os = "linux")]
            let (drm_root, proc_root) = (Path::new(DRM_ROOT), Path::new(PROC_ROOT));
            #[cfg(not(target_os = "linux"))]
            let (drm_root, proc_root) = (
                Path::new("/nonexistent/multiview/no-drm-off-linux"),
                Path::new("/nonexistent/multiview/no-proc-off-linux"),
            );
            DrmRoot::at(drm_root).into_probe().with_proc_root(proc_root)
        }

        /// Override the proc root the fdinfo walk reads (a synthetic tree in
        /// tests; `/proc` in production). Returns the probe for chaining.
        #[must_use]
        pub fn with_proc_root(mut self, proc_root: &Path) -> Self {
            self.proc_root = proc_root.to_path_buf();
            self
        }

        /// Inject the clock the media pass reads to timestamp each snapshot at
        /// its walk (tests only — production always reads the real monotonic
        /// clock). Lets a test drive an exact snapshot-to-snapshot interval
        /// through the real `sample_all_with_media` / `LoadSource::poll` seam.
        #[cfg(test)]
        #[must_use]
        pub(crate) fn with_clock(mut self, clock: SnapshotClock) -> Self {
            self.clock = clock;
            self
        }

        /// Sample every visible device, then enrich each with the merged
        /// media-engine busy fraction derived from the caller's own PIDs'
        /// `/proc/<pid>/fdinfo`.
        ///
        /// This is the off-hot-path poll the engine's load thread calls (ADR-0017
        /// §2): it takes one sysfs `sample_all`, walks `/proc/<pid>/fdinfo` for
        /// `pids`, timestamps that snapshot at its read, differences the merged
        /// media-ns against the previous snapshot over the elapsed between the two
        /// snapshot instants, and folds the resulting `0.0..=1.0` fraction into
        /// **both** `enc_util_frac` and `dec_util_frac` of the matching device —
        /// AMD VCN4+ merges decode+encode into one figure, so we report the same
        /// combined term for both rather than fabricating a split
        /// ([gpu-monitoring §3.6]). A device for which no attributable media fd is
        /// seen, or the first poll (no prior snapshot), leaves those fields
        /// `None` (honest unknown). `&mut self` because the two-snapshot diff is
        /// stateful; it is **never** called on the data plane. The walk reads only
        /// the caller's own PIDs.
        #[must_use]
        pub fn sample_all_with_media(&mut self, pids: &[u32]) -> Vec<DeviceLoad> {
            let mut loads = self.sample_all();
            let snapshot = FdinfoMediaSnapshot::walk_proc(&self.proc_root, pids);
            // Timestamp the snapshot at its read (not at poll entry) so the
            // tracker's divisor matches the counter delta it divides (F1). The
            // clock is injectable so a unit test drives an exact interval; it is
            // the real monotonic clock in production.
            let snapshot_at = self.clock.now();
            for load in &mut loads {
                let pdev = load.device_id.stable_id();
                if let Some(frac) =
                    self.media_tracker
                        .merged_media_frac(pdev, &snapshot, snapshot_at)
                {
                    // VCN4+ merges decode+encode — report the combined media term
                    // for both engines, never a fabricated per-engine split.
                    load.enc_util_frac = Some(frac);
                    load.dec_util_frac = Some(frac);
                }
            }
            loads
        }

        /// Like [`SysfsLoadProbe::sample_all_with_media`], but additionally folds
        /// the **Intel i915 PMU** per-engine media busy fraction into any Intel
        /// device whose enc/dec util the per-process fdinfo walk left unknown.
        ///
        /// This is the Intel fallback path: where DRM fdinfo enc/dec counters are
        /// unavailable (e.g. no attributable media fd, or a kernel that does not
        /// expose them per-process), the whole-device i915 PMU — owned by the
        /// `multiview-i915pmu` FFI leaf crate (the only place the
        /// `perf_event_open` syscall lives) — supplies a merged media busy figure
        /// via [`IntelPmuMediaTracker`]. The fdinfo term is preferred when present
        /// (it attributes to our own work); the PMU only fills a field fdinfo left
        /// `None`, and only on Intel devices ([`fold_intel_pmu_media_frac`]). A
        /// device with no openable counter (no i915, perf denied) or on its first
        /// poll keeps the honest-unknown `None`. `&mut self` + `&mut pmu` because
        /// both diffs are stateful; this is the off-hot-path poll, never on the
        /// data plane and never blocking the engine.
        #[cfg(feature = "i915-pmu")]
        #[must_use]
        pub fn sample_all_with_media_and_pmu(
            &mut self,
            pids: &[u32],
            pmu: &mut super::IntelPmuMediaTracker,
        ) -> Vec<DeviceLoad> {
            let mut loads = self.sample_all_with_media(pids);
            for load in &mut loads {
                if load.device_id.vendor() == Vendor::Intel {
                    let frac = pmu.merged_media_frac(&load.device_id);
                    super::fold_intel_pmu_media_frac(load, frac);
                }
            }
            loads
        }
    }

    impl LoadProbe for SysfsLoadProbe {
        fn devices(&self) -> Vec<DeviceId> {
            self.root
                .classify_cards()
                .into_iter()
                .map(|dirs| dirs.device_id())
                .collect()
        }

        fn sample(&self, device: &DeviceId) -> LoadSample {
            // Re-resolve the device dir by stable id (identity is the PCI bus id,
            // not the cardN index, which can reorder). If it has gone, report
            // Unavailable rather than mislabel another card.
            match self
                .root
                .classify_cards()
                .into_iter()
                .find(|dirs| &dirs.device_id() == device)
            {
                Some(dirs) => match read_device_load_from(&dirs) {
                    Some(load) => LoadSample::Ready(load),
                    None => LoadSample::Unavailable {
                        reason: "DRM device directory vanished during sample",
                    },
                },
                None => LoadSample::Unavailable {
                    reason: "DRM device no longer present",
                },
            }
        }
    }

    /// A bounded, off-hot-path [`LoadSource`] over [`SysfsLoadProbe`] that folds
    /// the per-process DRM fdinfo merged media-engine term into each AMD/Intel
    /// [`DeviceLoad`] (ADR-0017 ENG-4b).
    ///
    /// [`SysfsLoadProbe::sample_all_with_media`] is stateful (`&mut self`: it
    /// differences two `/proc/<pid>/fdinfo` snapshots and times each at its read)
    /// and needs the caller's own PIDs — which the object-safe
    /// [`LoadSource::poll`]`(&self)` seam does not carry. This poller owns that
    /// state behind a [`Mutex`] (interior mutability for the `&mut` media pass)
    /// and drives it once per metrics tick; the snapshot-to-snapshot interval is
    /// measured inside the probe's media tracker, so the poller holds no timing
    /// state of its own. It runs on the `multiview-cli` system-metrics task (off
    /// the output-clock thread), so the lock is uncontended and never touches the
    /// data plane (inv #1 / #10). A poisoned lock (a prior panic) or a no-DRM host
    /// degrades to an empty snapshot — never a panic, never a fabricated device.
    /// The fdinfo walk reads only our **own** PIDs.
    #[derive(Debug)]
    pub struct SysfsLoadPoller {
        probe: Mutex<SysfsLoadProbe>,
        self_pids: Vec<u32>,
    }

    impl SysfsLoadPoller {
        /// Try to construct the poller bound to the real `/sys/class/drm` +
        /// `/proc` roots, walking this process's own PID for the fdinfo media
        /// term.
        ///
        /// Returns `None` when no DRM GPU is visible (a no-DRM host / CI) so the
        /// caller cleanly falls back to the no-GPU source — mirroring the NVML
        /// poller's `try_init`. The one-off `/sys/class/drm` enumeration runs at
        /// construction, off the output-clock thread.
        #[must_use]
        pub fn try_init() -> Option<Self> {
            let probe = SysfsLoadProbe::new();
            if probe.devices().is_empty() {
                return None;
            }
            Some(Self::from_probe(probe, vec![std::process::id()]))
        }

        /// Wrap a probe with the PID set the fdinfo walk targets.
        ///
        /// `self_pids` are the caller's own processes (we only ever read our own
        /// `/proc/<pid>/fdinfo`). Tests inject a synthetic probe + fixed PIDs.
        #[must_use]
        pub(crate) fn from_probe(probe: SysfsLoadProbe, self_pids: Vec<u32>) -> Self {
            Self {
                probe: Mutex::new(probe),
                self_pids,
            }
        }
    }

    impl LoadSource for SysfsLoadPoller {
        fn poll(&self) -> Vec<DeviceLoad> {
            // Off the output-clock thread; the lock is uncontended. The media
            // tracker inside the probe times each snapshot at its read and
            // differences over the snapshot-to-snapshot interval, so the poll
            // itself holds no timing state. A poisoned lock degrades to an empty
            // snapshot (never a panic, never a fabricated device).
            let Ok(mut probe) = self.probe.lock() else {
                return Vec::new();
            };
            probe.sample_all_with_media(&self.self_pids)
        }
    }

    /// A bound DRM sysfs root (`/sys/class/drm`, or a test tree) the probe walks.
    ///
    /// Holding the root as data (rather than the hard-coded path) is what lets
    /// the `cardN` walk and the per-file reads run against a synthetic temp tree
    /// in unit tests with no real GPU — the live `/sys` read is the same code on
    /// the production root.
    #[derive(Debug, Clone)]
    pub(crate) struct DrmRoot {
        path: PathBuf,
    }

    impl DrmRoot {
        /// Bind a DRM root at `path`.
        #[must_use]
        pub(crate) fn at(path: &Path) -> Self {
            Self {
                path: path.to_path_buf(),
            }
        }

        /// Consume the root into a [`SysfsLoadProbe`] over it.
        ///
        /// The proc root defaults to a non-existent path (so the fdinfo walk is
        /// empty); a test that exercises the media path chains
        /// [`SysfsLoadProbe::with_proc_root`] onto a synthetic proc tree.
        #[must_use]
        pub(crate) fn into_probe(self) -> SysfsLoadProbe {
            SysfsLoadProbe {
                root: self,
                proc_root: PathBuf::from("/nonexistent/multiview/no-proc-for-test-root"),
                media_tracker: FdinfoMediaTracker::default(),
                clock: SnapshotClock::default(),
            }
        }

        /// Classify every `cardN` directory under the root as a GPU device,
        /// skipping render-only nodes and non-GPU cards.
        ///
        /// A missing/unreadable root yields an empty vector (the off-Linux /
        /// no-DRM fallback), never a panic.
        fn classify_cards(&self) -> Vec<DeviceDirs> {
            let Ok(entries) = std::fs::read_dir(&self.path) else {
                return Vec::new();
            };
            let mut cards: Vec<DeviceDirs> = entries
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name();
                    let name = name.to_str()?;
                    // Only `cardN` nodes carry the device metrics; `renderD*` and
                    // `controlD*` are skipped.
                    if !is_card_node(name) {
                        return None;
                    }
                    DeviceDirs::for_card(&self.path, name)
                })
                .collect();
            // Deterministic order so the `index` tie-breaker is stable per boot.
            cards.sort_by(|a, b| a.card.cmp(&b.card));
            cards
        }
    }

    /// Whether a DRM node name is a `cardN` primary node (not `renderD*`).
    ///
    /// Matches `card` followed by at least one ASCII digit and nothing else, so
    /// `card0`/`card12` match but `cardstuff` and `renderD128` do not.
    fn is_card_node(name: &str) -> bool {
        let Some(rest) = name.strip_prefix("card") else {
            return false;
        };
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
    }

    /// The resolved sysfs locations + identity for one classified GPU card.
    ///
    /// Built by [`DeviceDirs::for_card`], which reads the card's PCI `vendor` id
    /// to classify the silicon family and its `uevent` `PCI_SLOT_NAME` for the
    /// stable bus id. Holding the `device` dir lets [`read_device_load_from`]
    /// read each metric file relative to it.
    #[derive(Debug, Clone)]
    pub(crate) struct DeviceDirs {
        card: String,
        device_dir: PathBuf,
        vendor: Vendor,
        stable_id: String,
        index: u32,
    }

    impl DeviceDirs {
        /// Classify `cardN` under `root`, returning `None` when the card is not a
        /// recognised GPU silicon family (or its `vendor` file is unreadable).
        ///
        /// The stable id is the PCI bus id from `device/uevent`
        /// (`PCI_SLOT_NAME=`); if that is absent the card name is used as a
        /// last-resort id so the device is still distinct (it is never the
        /// load-bearing pin key in that degraded case, but enumeration stays
        /// non-panicking).
        #[must_use]
        pub(crate) fn for_card(root: &Path, card: &str) -> Option<Self> {
            let device_dir = root.join(card).join("device");
            let vendor_raw = std::fs::read_to_string(device_dir.join("vendor")).ok()?;
            let vendor = classify_pci_vendor(&vendor_raw)?;
            let stable_id = std::fs::read_to_string(device_dir.join("uevent"))
                .ok()
                .and_then(|u| parse_pci_slot_name(&u))
                .unwrap_or_else(|| card.to_owned());
            let index = parse_card_index(card).unwrap_or(0);
            Some(Self {
                card: card.to_owned(),
                device_dir,
                vendor,
                stable_id,
                index,
            })
        }

        /// The stable [`DeviceId`] for this card: vendor + the PCI bus id (the
        /// placement + pin key), with the `cardN` index as the tie-breaker.
        ///
        /// The PCI slot is also attached as the non-identity cross-probe key so
        /// the scanout KMS probe reconciles a DRM card to this `DeviceId` by the
        /// same `pci_bus_id` path it uses for an NVML (UUID-keyed) device.
        #[must_use]
        pub(crate) fn device_id(&self) -> DeviceId {
            DeviceId::new(self.vendor, self.stable_id.clone(), self.index)
                .with_pci_bus_id(self.stable_id.clone())
        }
    }

    /// Read a [`DeviceLoad`] from a classified card's sysfs files.
    ///
    /// Each metric is read independently: `gpu_busy_percent` -> `gpu_busy_frac`,
    /// `mem_info_vram_total`/`_used` -> VRAM bytes. A file that is absent or
    /// garbled leaves that field `None` (honest unknown). Returns `None` only if
    /// the device directory itself has vanished between classification and read,
    /// so a caller can surface `Unavailable`.
    #[must_use]
    pub(crate) fn read_device_load_from(dirs: &DeviceDirs) -> Option<DeviceLoad> {
        if !dirs.device_dir.exists() {
            return None;
        }
        let read = |name: &str| std::fs::read_to_string(dirs.device_dir.join(name)).ok();
        let gpu_busy_frac = read("gpu_busy_percent")
            .as_deref()
            .and_then(parse_gpu_busy_percent);
        let vram_total_bytes = read("mem_info_vram_total")
            .as_deref()
            .and_then(parse_vram_bytes);
        let vram_used_bytes = read("mem_info_vram_used")
            .as_deref()
            .and_then(parse_vram_bytes);
        Some(DeviceLoad {
            device_id: dirs.device_id(),
            gpu_busy_frac,
            vram_used_bytes,
            vram_total_bytes,
            // Per-engine enc/dec is not in sysfs: AMD is per-process fdinfo
            // (merged from VCN4), Intel is the i915 PMU. Honest unknown here.
            enc_util_frac: None,
            dec_util_frac: None,
            // NVENC session count is NVIDIA-only.
            nvenc_session_count: None,
            // amdgpu/i915 do not expose a separate compute queue % in sysfs.
            compute_busy_frac: None,
        })
    }

    /// Classify a `/sys/.../device/vendor` string into a GPU [`Vendor`].
    ///
    /// The file holds the PCI vendor id as `0x`-prefixed lower-case hex with a
    /// trailing newline (e.g. `"0x1002\n"`). Returns `None` for an unknown /
    /// non-GPU id or unparsable text — never a wrong guess.
    #[must_use]
    pub(crate) fn classify_pci_vendor(raw: &str) -> Option<Vendor> {
        let trimmed = raw.trim();
        let hex = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))?;
        let id = u16::from_str_radix(hex, 16).ok()?;
        match id {
            PCI_VENDOR_NVIDIA => Some(Vendor::Nvidia),
            PCI_VENDOR_AMD => Some(Vendor::Amd),
            PCI_VENDOR_INTEL => Some(Vendor::Intel),
            _ => None,
        }
    }

    /// Parse a `gpu_busy_percent` sysfs string (a `0..=100` integer) into a
    /// clamped `0.0..=1.0` busy fraction. `None` on unparsable/empty text.
    #[must_use]
    pub(crate) fn parse_gpu_busy_percent(raw: &str) -> Option<f32> {
        let value: u32 = raw.trim().parse().ok()?;
        Some(percent_to_busy_frac(value))
    }

    /// Parse a `mem_info_vram_{total,used}` sysfs string (a decimal byte count)
    /// into a `u64`. `None` on unparsable/empty text.
    #[must_use]
    pub(crate) fn parse_vram_bytes(raw: &str) -> Option<u64> {
        raw.trim().parse().ok()
    }

    /// Extract the stable PCI bus id from a `device/uevent` file body.
    ///
    /// The kernel writes a `PCI_SLOT_NAME=<domain>:<bus>:<dev>.<func>` line
    /// (e.g. `PCI_SLOT_NAME=0000:03:00.0`). Returns the value, or `None` if the
    /// line is absent.
    fn parse_pci_slot_name(uevent: &str) -> Option<String> {
        uevent
            .lines()
            .find_map(|line| line.trim().strip_prefix("PCI_SLOT_NAME="))
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }

    /// Parse the `N` from a `cardN` node name into a `u32` enumeration index
    /// (a deterministic tie-breaker only, never identity).
    fn parse_card_index(card: &str) -> Option<u32> {
        card.strip_prefix("card")?.parse().ok()
    }

    /// One parsed DRM fdinfo snapshot: the per-engine nanosecond busy counters
    /// for a single client fd (`/proc/<pid>/fdinfo/<drm_fd>`).
    ///
    /// DRM fdinfo reports `drm-engine-<class>:\t<ns> ns` lines whose counter is
    /// monotonically increasing total busy-ns for that engine class. A busy
    /// fraction is derived by differencing two snapshots a known wall interval
    /// apart ([gpu-monitoring §2.1, A2]). This is the AMD per-process media term
    /// (merged decode+encode from VCN4); kept as a pure, testable parser that a
    /// follow-up wires to the live `/proc/<pid>/fdinfo` walk.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct FdinfoSample {
        media_ns: Option<u64>,
    }

    impl FdinfoSample {
        /// Parse the merged media-engine busy-ns from an fdinfo file body.
        ///
        /// Sums every `drm-engine-<class>` line whose class is a media engine
        /// (`enc`, `dec`, or the post-VCN4 merged `media`) into one combined
        /// figure. Always returns a sample (with `media_ns = None` when no media
        /// engine line is present) so the difference helper can report unknown.
        #[must_use]
        pub fn parse(body: &str) -> Option<Self> {
            let mut media_ns: Option<u64> = None;
            for line in body.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let key = key.trim();
                let Some(class) = key.strip_prefix("drm-engine-") else {
                    continue;
                };
                if !is_media_engine_class(class) {
                    continue;
                }
                // The value is `<ns> ns`; take the leading integer.
                let Some(ns_text) = value.split_whitespace().next() else {
                    continue;
                };
                let Ok(ns) = ns_text.parse::<u64>() else {
                    continue;
                };
                media_ns = Some(media_ns.unwrap_or(0).saturating_add(ns));
            }
            Some(Self { media_ns })
        }
    }

    /// Whether a `drm-engine-<class>` class name names a media (encode/decode)
    /// engine that AMD VCN4+ merges into one figure.
    fn is_media_engine_class(class: &str) -> bool {
        matches!(class, "enc" | "dec" | "media")
    }

    /// Derive the merged AMD media-engine busy fraction from two fdinfo snapshots
    /// `interval_ns` apart.
    ///
    /// `(later.media_ns - earlier.media_ns) / interval_ns`, clamped to
    /// `0.0..=1.0`. Returns `None` when either snapshot lacks a media counter
    /// (unknown, not a fabricated zero), when the interval is non-positive (a
    /// divide guard), or when the counter went backwards (a reset — unknown this
    /// tick). Never panics, never blocks.
    #[must_use]
    pub fn parse_fdinfo_merged_media_frac(
        earlier: &FdinfoSample,
        later: &FdinfoSample,
        interval_ns: u64,
    ) -> Option<f32> {
        if interval_ns == 0 {
            return None;
        }
        let earlier_ns = earlier.media_ns?;
        let later_ns = later.media_ns?;
        let delta = later_ns.checked_sub(earlier_ns)?;
        Some(super::busy_ratio(delta, interval_ns))
    }

    /// Extract the owning device's PCI bus id from a DRM fdinfo file body.
    ///
    /// The DRM common fdinfo schema reports the device a client fd belongs to as
    /// a `drm-pdev:\t<domain>:<bus>:<dev>.<func>` line (e.g. `0000:03:00.0`) —
    /// the **same** PCI bus id the sysfs walk keys a [`DeviceId`] by
    /// ([gpu-monitoring §2.1], the `PCI_SLOT_NAME`), so a walked fd attributes to
    /// a card. Returns `None` when the line is absent or its value is empty
    /// (the fd cannot be attributed — unknown, never a wrong guess).
    #[must_use]
    pub fn parse_fdinfo_pdev(body: &str) -> Option<String> {
        body.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.trim() != "drm-pdev" {
                return None;
            }
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_owned())
            }
        })
    }

    /// One captured pass over the process set's DRM fdinfo: the merged
    /// media-engine busy-ns summed **per device** (keyed by the `drm-pdev:` PCI
    /// bus id) across every render fd our own PIDs hold.
    ///
    /// Built by [`FdinfoMediaSnapshot::walk_proc`] (the live `/proc/<pid>/fdinfo`
    /// read on Linux) or, in tests, by feeding fdinfo bodies through
    /// [`FdinfoMediaSnapshot::accumulate_fd`]. A busy fraction is derived by
    /// differencing two snapshots a known wall interval apart — see
    /// [`FdinfoMediaTracker`]. AMD VCN4+ merges decode+encode into one media
    /// figure, so this is the **combined** enc+dec term (the brief's per-process
    /// AMD fallback, [gpu-monitoring §3.6]); pre-VCN4 the separate `-enc`/`-dec`
    /// keys still sum into the same merged total here.
    ///
    /// An fd with no `drm-pdev:` (unattributable) or no media counter
    /// contributes nothing — never a fabricated zero, never a panic.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct FdinfoMediaSnapshot {
        /// PCI bus id -> summed merged media-engine busy-ns across the fds.
        per_device_ns: HashMap<String, u64>,
    }

    impl FdinfoMediaSnapshot {
        /// Fold one DRM fdinfo file body into the snapshot.
        ///
        /// Attributes the fd to its device via `drm-pdev:` and adds the merged
        /// media-engine busy-ns ([`FdinfoSample::parse`]) to that device's
        /// running sum. A body with no pdev or no media counter is a no-op.
        pub fn accumulate_fd(&mut self, body: &str) {
            let Some(pdev) = parse_fdinfo_pdev(body) else {
                return;
            };
            let Some(sample) = FdinfoSample::parse(body) else {
                return;
            };
            let Some(media_ns) = sample.media_ns else {
                return;
            };
            self.per_device_ns
                .entry(pdev)
                .and_modify(|ns| *ns = ns.saturating_add(media_ns))
                .or_insert(media_ns);
        }

        /// The merged media-engine busy-ns summed for one device, or `None` if no
        /// attributable media fd for it was seen (honest unknown).
        #[must_use]
        pub fn media_ns(&self, pdev: &str) -> Option<u64> {
            self.per_device_ns.get(pdev).copied()
        }

        /// Walk `<proc_root>/<pid>/fdinfo/*` for each PID in `pids`, summing the
        /// merged media-engine busy-ns per device into a fresh snapshot.
        ///
        /// `proc_root` is `/proc` in production and a synthetic tree in tests.
        /// Each `fdinfo/<fd>` file is read with plain `std::fs` (no `unsafe`, no
        /// native lib); a file that is absent, unreadable, or not a DRM client fd
        /// contributes nothing. A missing root or pid directory yields an empty
        /// snapshot — graceful absence, never a panic and never a block. Only the
        /// caller's **own** PIDs are walked (we own our processes,
        /// [gpu-monitoring §3.6]); no other process's fds are read.
        #[must_use]
        pub fn walk_proc(proc_root: &Path, pids: &[u32]) -> Self {
            let mut snapshot = Self::default();
            for &pid in pids {
                let fdinfo_dir = proc_root.join(pid.to_string()).join("fdinfo");
                let Ok(entries) = std::fs::read_dir(&fdinfo_dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let Ok(body) = std::fs::read_to_string(entry.path()) else {
                        continue;
                    };
                    snapshot.accumulate_fd(&body);
                }
            }
            snapshot
        }
    }

    /// The stateful two-snapshot differ that turns a sequence of
    /// [`FdinfoMediaSnapshot`]s into a per-device merged-media busy fraction.
    ///
    /// The DRM media-engine counter is a monotonically increasing busy-ns total,
    /// so a fraction needs **two** samples a known wall interval apart. The
    /// tracker holds the previous snapshot **and the instant it was captured**;
    /// each poll differences the new snapshot against it over the elapsed between
    /// the two capture instants ([`parse_fdinfo_merged_media_frac`]) and then
    /// retains the new snapshot + instant for next time. Timing each snapshot at
    /// its own read (rather than at poll entry) keeps the divisor matched to the
    /// counter delta it divides — variable `/sys`+`/proc` traversal latency would
    /// otherwise over/under-report utilization. The first poll (no prior) is
    /// `None` — unknown until two samples exist, never a fabricated zero. The
    /// tracker is the off-hot-path state the load poll thread owns (ADR-0017 §2);
    /// it does no I/O itself.
    #[derive(Debug, Clone, Default)]
    pub struct FdinfoMediaTracker {
        previous: Option<PrevSnapshot>,
    }

    /// The previous fdinfo media snapshot + the wall instant it was captured, so
    /// the next poll divides the counter delta by the elapsed between the two
    /// snapshot reads (not the poll-entry delta). Mirrors the i915-PMU
    /// `PrevSample { busy_ns, at }`.
    #[derive(Debug, Clone)]
    struct PrevSnapshot {
        snapshot: FdinfoMediaSnapshot,
        at: Instant,
    }

    impl FdinfoMediaTracker {
        /// Difference `latest` (captured at `latest_at`) against the retained
        /// previous snapshot for one device and return its merged media busy
        /// fraction (`0.0..=1.0`) over the elapsed between the two snapshot
        /// instants, then retain `latest` + `latest_at` for the next poll.
        ///
        /// Returns `None` on the first call (no prior), when this device had no
        /// attributable media fd in either snapshot, when the two snapshots share
        /// an instant (a zero-length interval — divide guard), or when the counter
        /// went backwards (a reset — unknown this tick). Never panics, never blocks.
        pub fn merged_media_frac(
            &mut self,
            pdev: &str,
            latest: &FdinfoMediaSnapshot,
            latest_at: Instant,
        ) -> Option<f32> {
            let frac = self.previous.as_ref().and_then(|prev| {
                // The divisor is the elapsed between the two SNAPSHOT reads —
                // saturating so a backwards or implausibly long gap never panics
                // or overflows (mirrors the i915-PMU `elapsed_ns`).
                let interval_ns =
                    u64::try_from(latest_at.saturating_duration_since(prev.at).as_nanos())
                        .unwrap_or(u64::MAX);
                let earlier = FdinfoSample {
                    media_ns: prev.snapshot.media_ns(pdev),
                };
                let later = FdinfoSample {
                    media_ns: latest.media_ns(pdev),
                };
                parse_fdinfo_merged_media_frac(&earlier, &later, interval_ns)
            });
            self.previous = Some(PrevSnapshot {
                snapshot: latest.clone(),
                at: latest_at,
            });
            frac
        }
    }
}

// ----------------------------------------------------------------------------
// Intel i915 PMU per-engine media-util fallback (ADR-0017).
//
// Where the per-process DRM fdinfo enc/dec counters are unavailable on Intel,
// the whole-device i915 PMU exposes per-engine busy-ns via `perf_event_open(2)`.
// That syscall needs `unsafe`, so it is owned by the `multiview-i915pmu` FFI leaf
// crate; this module folds its busy fraction into an Intel `DeviceLoad`'s
// enc/dec util WITHOUT this crate ever performing `unsafe` (it stays
// `forbid(unsafe_code)`). The fold decision + the two-snapshot diff are pure and
// tested; the live counter open/read is gated on a real i915 + perf permission.
// ----------------------------------------------------------------------------

#[cfg(feature = "i915-pmu")]
pub use self::i915_pmu::{fold_intel_pmu_media_frac, IntelPmuMediaTracker};

#[cfg(feature = "i915-pmu")]
mod i915_pmu {
    use super::{DeviceId, DeviceLoad, Vendor};
    #[cfg(target_os = "linux")]
    use multiview_i915pmu::read_pmu_type;
    use multiview_i915pmu::{
        busy_fraction, engine_busy_config, I915PmuCounter, ENGINE_CLASS_VIDEO,
        ENGINE_CLASS_VIDEO_ENHANCE,
    };
    use std::collections::HashMap;
    #[cfg(target_os = "linux")]
    use std::path::Path;
    use std::time::Instant;

    /// The sysfs root under which the kernel publishes each i915 PMU's dynamic
    /// `type` (one `i915` or `i915_<pci-bus-id>` directory per GPU).
    #[cfg(target_os = "linux")]
    const SYS_DEVICES: &str = "/sys/devices";

    /// Fold a PMU-derived merged media busy fraction into an Intel device's
    /// `enc_util_frac`/`dec_util_frac`, **only** where they are still unknown.
    ///
    /// The contract (ADR-0017, [gpu-monitoring §3.6]): the per-process DRM fdinfo
    /// term is preferred when present (it attributes to our own work); the i915
    /// PMU is the *fallback* whole-device figure used **only** when fdinfo left a
    /// field `None`. It is applied to Intel devices only (the PMU is i915), and a
    /// `None` fraction (no counter, first poll, denied) is a no-op — the field
    /// stays honest-unknown, never a fabricated zero. Pure and total.
    pub fn fold_intel_pmu_media_frac(load: &mut DeviceLoad, frac: Option<f32>) {
        if load.device_id.vendor() != Vendor::Intel {
            return;
        }
        let Some(frac) = frac else {
            return;
        };
        // Only fill what fdinfo could not: never overwrite a per-process reading.
        if load.enc_util_frac.is_none() {
            load.enc_util_frac = Some(frac);
        }
        if load.dec_util_frac.is_none() {
            load.dec_util_frac = Some(frac);
        }
    }

    /// One device's previous PMU busy-ns sample + the wall instant it was taken,
    /// so the next poll can difference busy-ns over the elapsed wall-ns.
    #[derive(Debug, Clone, Copy)]
    struct PrevSample {
        busy_ns: u64,
        at: Instant,
    }

    /// The stateful two-snapshot differ that turns a sequence of i915 PMU
    /// per-engine busy-ns reads into a per-device merged-media busy fraction.
    ///
    /// The i915 PMU busy counter is a monotonically increasing busy-ns total, so
    /// a fraction needs **two** reads a known wall interval apart. The tracker
    /// holds, per device, the open VCS (video) + VECS (video-enhance) counters
    /// and the previous summed busy-ns + its instant; each poll reads the live
    /// counters, differences against the retained sample over the measured
    /// elapsed wall-ns, and retains the new sample. The first poll for a device
    /// (no prior) is `None` — unknown until two samples exist, never a fabricated
    /// zero. The tracker is the off-hot-path state the load poll thread owns
    /// (ADR-0017 §2); the counters are opened lazily per device and closed on
    /// `Drop`.
    ///
    /// Opening a counter needs a real i915 + perf permission; where that is
    /// unavailable the per-device entry stays without counters and
    /// [`IntelPmuMediaTracker::merged_media_frac`] returns `None` — the honest
    /// fallback that leaves the sysfs probe's Intel enc/dec fields unknown.
    #[derive(Debug, Default)]
    pub struct IntelPmuMediaTracker {
        per_device: HashMap<String, DeviceCounters>,
    }

    /// The opened-or-absent PMU counters + previous sample for one device.
    #[derive(Debug, Default)]
    struct DeviceCounters {
        /// The video (VCS) busy-ns counter, if it opened.
        video: Option<I915PmuCounter>,
        /// The video-enhance (VECS) busy-ns counter, if it opened.
        video_enhance: Option<I915PmuCounter>,
        /// Whether an open was already attempted (so we do not retry every poll
        /// on a host that denies it — a denied open stays denied).
        opened: bool,
        /// The previous summed busy-ns + its wall instant.
        prev: Option<PrevSample>,
    }

    impl IntelPmuMediaTracker {
        /// Difference the live i915 PMU media-engine busy-ns for one Intel device
        /// against the retained previous sample and return its merged media busy
        /// fraction (`0.0..=1.0`), then retain the new sample for the next poll.
        ///
        /// `device` must be an Intel device; its `stable_id` keys the per-device
        /// counters. The merged figure sums the video (VCS) and video-enhance
        /// (VECS) engines — Intel meters them separately but Multiview reports one
        /// combined media term, mirroring the AMD merged fallback. Returns `None`
        /// on the first poll (no prior), when no counter could be opened (no i915,
        /// perf denied), or when a read failed this tick — honest unknown, never a
        /// fabricated zero. Never panics, never blocks on the engine.
        pub fn merged_media_frac(&mut self, device: &DeviceId) -> Option<f32> {
            if device.vendor() != Vendor::Intel {
                return None;
            }
            let key = device.stable_id().to_owned();
            let counters = self.per_device.entry(key).or_default();
            counters.ensure_opened(device);

            let now = Instant::now();
            let busy_ns = counters.read_summed_busy_ns()?;
            let frac = counters.prev.and_then(|prev| {
                let interval_ns = elapsed_ns(prev.at, now);
                busy_fraction(prev.busy_ns, busy_ns, interval_ns)
            });
            counters.prev = Some(PrevSample { busy_ns, at: now });
            frac
        }
    }

    impl DeviceCounters {
        /// Open the VCS + VECS busy-ns counters for `device` once (idempotent).
        ///
        /// Resolves the device's i915 PMU `type` from sysfs, then opens a counter
        /// per engine class. A device whose PMU type cannot be resolved, or whose
        /// open is denied, leaves the counters `None`; the attempt is recorded so
        /// it is not retried on every poll.
        fn ensure_opened(&mut self, device: &DeviceId) {
            if self.opened {
                return;
            }
            self.opened = true;
            let Some(pmu_type) = resolve_pmu_type(device) else {
                return;
            };
            self.video = I915PmuCounter::open(pmu_type, engine_busy_config(ENGINE_CLASS_VIDEO, 0));
            self.video_enhance =
                I915PmuCounter::open(pmu_type, engine_busy_config(ENGINE_CLASS_VIDEO_ENHANCE, 0));
        }

        /// Sum the live busy-ns of the opened media counters, or `None` if no
        /// counter is open or a read failed this tick.
        fn read_summed_busy_ns(&self) -> Option<u64> {
            let video = self.video.as_ref().and_then(I915PmuCounter::read_busy_ns);
            let vecs = self
                .video_enhance
                .as_ref()
                .and_then(I915PmuCounter::read_busy_ns);
            match (video, vecs) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }
        }
    }

    /// Elapsed nanoseconds between two instants, saturating into a `u64`.
    ///
    /// `Instant::saturating_duration_since` never goes backwards (so the busy
    /// fraction's interval is always non-negative); the ns count saturates at
    /// `u64::MAX` for an implausibly long gap rather than overflowing.
    fn elapsed_ns(earlier: Instant, later: Instant) -> u64 {
        let nanos = later.saturating_duration_since(earlier).as_nanos();
        u64::try_from(nanos).unwrap_or(u64::MAX)
    }

    /// Resolve an Intel device's dynamic i915 PMU `type` number from sysfs.
    ///
    /// The kernel publishes the PMU under `/sys/devices/i915` (single GPU) or
    /// `/sys/devices/i915_<pci-bus-id>` (one per GPU; the bus id has `:`/`.`
    /// rewritten to `_`). We prefer the device-specific directory matching the
    /// `DeviceId`'s PCI bus id, then fall back to the plain `i915` directory.
    /// Returns `None` off-Linux or where no i915 PMU is present.
    #[cfg(target_os = "linux")]
    fn resolve_pmu_type(device: &DeviceId) -> Option<u32> {
        let root = Path::new(SYS_DEVICES);
        // The per-device PMU dir name rewrites the PCI separators to `_`
        // (e.g. `0000:03:00.0` -> `i915_0000_03_00.0`).
        let device_dir = format!("i915_{}", device.stable_id().replace([':'], "_"));
        for candidate in [device_dir.as_str(), "i915"] {
            let type_path = root.join(candidate).join("type");
            if let Some(t) = read_pmu_type(&type_path) {
                return Some(t);
            }
        }
        None
    }

    /// Off-Linux there is no i915 PMU sysfs; the type is always unresolved.
    #[cfg(not(target_os = "linux"))]
    fn resolve_pmu_type(_device: &DeviceId) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    fn nv(id: &str, index: u32) -> DeviceId {
        DeviceId::new(Vendor::Nvidia, id, index)
    }

    #[test]
    fn null_poller_is_a_load_source_with_no_devices() {
        // The always-compiled no-GPU poller: a working `LoadSource` that yields
        // zero devices on every poll (the honest "no accelerator visible" state),
        // so the pure-Rust default build has a real poller to inject.
        let poller = NullLoadPoller::new();
        let loads: Vec<DeviceLoad> = poller.poll();
        assert!(
            loads.is_empty(),
            "the null poller must report no devices, got {}",
            loads.len()
        );
        // Usable behind the object-safe `LoadSource` seam the CLI injects.
        let dynamic: &dyn LoadSource = &poller;
        assert!(dynamic.poll().is_empty());
        // The default `device_perf` honestly reports no perf signals (so the CLI
        // candidate falls back to the conservative DEFAULT_PERF_CLASS), never a
        // fabricated device.
        assert!(dynamic.device_perf().is_empty());
    }

    #[test]
    fn pci_bus_id_is_carried_but_is_not_part_of_identity() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        // The PCI bus id is the cross-probe MATCHING key (scanout ↔ render-node),
        // not part of `(vendor, stable_id)` identity. Two DeviceIds with the same
        // identity but different/absent pci must stay equal + hash-equal so adding
        // it never changes the placement key, and the normalized accessor maps
        // both NVML's 8-digit-domain and sysfs's 4-digit-domain onto one form.
        let bare = nv("GPU-uuid", 0);
        let with_pci = nv("GPU-uuid", 0).with_pci_bus_id("00000000:03:00.0");
        assert_eq!(bare, with_pci, "pci is NOT part of identity equality");
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        bare.hash(&mut h1);
        with_pci.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish(), "pci is NOT part of identity hash");
        // The raw value is preserved; the normalized accessor canonicalizes it.
        assert_eq!(with_pci.pci_bus_id(), Some("00000000:03:00.0"));
        assert_eq!(
            with_pci.pci_bus_id_normalized().as_deref(),
            Some("0000:03:00.0")
        );
        assert_eq!(bare.pci_bus_id(), None);
        assert_eq!(bare.pci_bus_id_normalized(), None);
    }

    #[test]
    fn device_target_default_is_none_for_a_source_with_no_handles() {
        // A source with no per-device hardware-handle signal (the no-GPU poller,
        // any vendor that exposes none) cannot pin a device: `device_target` must
        // return `None` so the consumer keeps its default adapter path (the
        // ADR-0035 Tier-1 graceful-fallback contract, GPU-free build).
        let poller = NullLoadPoller::new();
        let source: &dyn LoadSource = &poller;
        assert!(source.device_target(&nv("GPU-uuid", 0)).is_none());
    }

    #[test]
    fn gpu_target_info_default_is_all_unknown() {
        // The honest starting point: every hardware handle `None` (never a
        // fabricated value), so a projection of it pins nothing.
        let info = GpuTargetInfo::default();
        assert_eq!(info.pci_bus_id, None);
        assert_eq!(info.vendor_id, None);
        assert_eq!(info.device_id, None);
        assert_eq!(info.name, None);
        assert_eq!(info.cuda_ordinal, None);
    }

    #[test]
    fn load_poller_is_a_load_source_over_its_probe() {
        // A `LoadPoller<P>` (the existing probe-wrapping poller) is itself a
        // `LoadSource`, so the same injection seam carries a real vendor probe.
        // We drive it with a deterministic in-memory probe (no GPU): the
        // `LoadSource::poll` must return exactly what the probe sees.
        struct OneDeviceProbe(DeviceLoad);
        impl LoadProbe for OneDeviceProbe {
            fn devices(&self) -> Vec<DeviceId> {
                vec![self.0.device_id.clone()]
            }
            fn sample(&self, _device: &DeviceId) -> LoadSample {
                LoadSample::Ready(self.0.clone())
            }
        }
        let mut load = DeviceLoad::unknown(nv("GPU-poll", 0));
        load.gpu_busy_frac = Some(0.5);
        let poller = LoadPoller::new(OneDeviceProbe(load.clone()), PollInterval::default());
        let via_source: &dyn LoadSource = &poller;
        let loads = via_source.poll();
        assert_eq!(loads.len(), 1);
        assert_eq!(loads.first().map(|l| l.gpu_busy_frac), Some(Some(0.5)));
    }

    #[test]
    fn poll_self_share_defaults_to_empty_for_the_null_source() {
        // The always-compiled no-GPU source attributes no per-process share: a
        // `LoadSource` with no per-process signal returns an empty share list (the
        // default-method contract), never a fabricated zero.
        let poller = NullLoadPoller::new();
        let dynamic: &dyn LoadSource = &poller;
        assert!(
            dynamic.poll_self_share().is_empty(),
            "the null source must report no per-process share"
        );
    }

    #[test]
    fn self_mem_sums_only_our_processes() {
        // Given a device's running-process list as (pid, used_bytes) pairs, our
        // share is the sum of the entries whose pid is ours; a co-tenant NVR pid is
        // excluded. Two of our contexts (graphics + compute) sum.
        let procs = [
            (1000_u32, Some(2_000_000_000_u64)), // ours (graphics)
            (1000_u32, Some(1_000_000_000_u64)), // ours (compute)
            (4242_u32, Some(8_000_000_000_u64)), // co-tenant NVR — excluded
        ];
        assert_eq!(
            self_mem_from_processes(&procs, 1000),
            Some(3_000_000_000),
            "our VRAM is the sum of OUR contexts, never the co-tenant's"
        );
    }

    #[test]
    fn self_mem_is_none_when_we_are_not_resident_or_mem_unknown() {
        // Not resident on this device => honest None (never a false zero).
        let procs = [(4242_u32, Some(8_000_000_000_u64))];
        assert_eq!(self_mem_from_processes(&procs, 1000), None);
        // Resident but the driver reports our memory as Unavailable (None) => None.
        let unknown = [(1000_u32, None)];
        assert_eq!(self_mem_from_processes(&unknown, 1000), None);
        // No processes at all => None.
        assert_eq!(self_mem_from_processes(&[], 1000), None);
    }

    #[test]
    fn self_sessions_counts_only_ours() {
        // Encoder-session owners as pids; our session count is how many are ours.
        let owners = [4242_u32, 1000, 1000, 9999];
        assert_eq!(
            self_sessions_from(&owners, 1000),
            2,
            "two of the four active encode sessions are ours"
        );
        // None of ours => an honest zero count (we DID query; we own none).
        assert_eq!(self_sessions_from(&[4242, 9999], 1000), 0);
    }

    #[test]
    fn self_util_averages_our_samples_as_a_fraction() {
        // process_utilization_stats yields (pid, percent) rows; our utilisation is
        // the mean of OUR rows, mapped 0-100% -> 0.0..=1.0. 40% and 60% -> 0.5.
        let samples = [(1000_u32, 40_u32), (1000_u32, 60_u32), (4242_u32, 90_u32)];
        let frac = self_util_avg_from_samples(&samples, 1000).expect("our rows present");
        assert!(
            (frac - 0.5).abs() < 1e-4,
            "mean of 40%+60% is 0.5, got {frac}"
        );
        // A >100% reading clamps; a single 100% row -> 1.0.
        let hot = [(1000_u32, 150_u32)];
        assert_eq!(self_util_avg_from_samples(&hot, 1000), Some(1.0));
    }

    #[test]
    fn self_util_is_none_when_we_have_no_samples() {
        // No row for our pid (idle / driver gave no per-process sample) => None,
        // never a fabricated 0.0.
        let samples = [(4242_u32, 90_u32)];
        assert_eq!(self_util_avg_from_samples(&samples, 1000), None);
        assert_eq!(self_util_avg_from_samples(&[], 1000), None);
    }

    #[test]
    fn self_share_unknown_is_all_none() {
        // The honest starting point: a `SelfShare` carries the device identity and
        // every per-process signal `None` until populated.
        let share = SelfShare::unknown(nv("GPU-x", 0));
        assert_eq!(share.device_id, nv("GPU-x", 0));
        assert!(share.compute_util.is_none());
        assert!(share.encoder_util.is_none());
        assert!(share.decoder_util.is_none());
        assert!(share.mem_used_bytes.is_none());
        assert!(share.encoder_sessions.is_none());
    }

    #[test]
    fn device_id_identity_ignores_index() {
        // Same vendor + stable id is the SAME device even if the enumeration
        // index reordered across a reboot.
        let a = nv("GPU-uuid-1", 0);
        let b = nv("GPU-uuid-1", 3);
        assert_eq!(a, b);
        // Different stable id is a different device.
        let c = nv("GPU-uuid-2", 0);
        assert_ne!(a, c);
    }

    #[test]
    fn unknown_load_is_all_none() {
        let load = DeviceLoad::unknown(nv("GPU-x", 0));
        assert!(load.gpu_busy_frac.is_none());
        assert!(load.vram_used_frac().is_none());
        assert!(load.vram_free_bytes().is_none());
        assert!(load.effective_compute_frac().is_none());
        assert!(load.nvenc_session_count.is_none());
    }

    #[test]
    fn vram_used_frac_is_clamped_unit_fraction() {
        let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
        load.vram_used_bytes = Some(6_000_000_000);
        load.vram_total_bytes = Some(12_000_000_000);
        let frac = load.vram_used_frac().expect("both bytes known");
        assert!((frac - 0.5).abs() < 1e-4, "half used => 0.5, got {frac}");
        assert_eq!(load.vram_free_bytes(), Some(6_000_000_000));
    }

    #[test]
    fn vram_frac_handles_transient_over_total() {
        // A transient sample reporting used > total must clamp, not exceed 1.0,
        // and free saturates at 0 rather than underflowing.
        let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
        load.vram_used_bytes = Some(13_000_000_000);
        load.vram_total_bytes = Some(12_000_000_000);
        assert_eq!(load.vram_used_frac(), Some(1.0));
        assert_eq!(load.vram_free_bytes(), Some(0));
    }

    #[test]
    fn vram_frac_unknown_when_total_zero() {
        let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
        load.vram_used_bytes = Some(0);
        load.vram_total_bytes = Some(0);
        assert!(load.vram_used_frac().is_none(), "zero total => unknown");
    }

    #[test]
    fn effective_compute_prefers_compute_over_gpu_busy() {
        let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
        load.gpu_busy_frac = Some(0.4);
        load.compute_busy_frac = Some(0.7);
        assert_eq!(load.effective_compute_frac(), Some(0.7));
        load.compute_busy_frac = None;
        assert_eq!(load.effective_compute_frac(), Some(0.4));
    }

    #[test]
    fn poll_interval_clamps_to_envelope() {
        assert_eq!(PollInterval::from_millis(10).as_millis(), 250);
        assert_eq!(PollInterval::from_millis(5000).as_millis(), 1000);
        assert_eq!(PollInterval::from_millis(500).as_millis(), 500);
        assert_eq!(PollInterval::default().as_millis(), 500);
    }

    /// A probe double that returns fixed samples — exercises the poller and the
    /// `sample_all` default without hardware.
    struct FakeProbe {
        loads: Vec<DeviceLoad>,
    }

    impl LoadProbe for FakeProbe {
        fn devices(&self) -> Vec<DeviceId> {
            self.loads.iter().map(|l| l.device_id.clone()).collect()
        }
        fn sample(&self, device: &DeviceId) -> LoadSample {
            self.loads
                .iter()
                .find(|l| &l.device_id == device)
                .cloned()
                .map_or(
                    LoadSample::Unavailable {
                        reason: "no such device",
                    },
                    LoadSample::Ready,
                )
        }
    }

    #[test]
    fn poller_samples_every_visible_device() {
        let loads = vec![
            DeviceLoad::unknown(nv("GPU-a", 0)),
            DeviceLoad::unknown(nv("GPU-b", 1)),
        ];
        let poller = LoadPoller::new(FakeProbe { loads }, PollInterval::default());
        let sampled = poller.poll();
        assert_eq!(sampled.len(), 2);
        assert_eq!(sampled[0].device_id, nv("GPU-a", 0));
    }

    #[test]
    fn sample_of_absent_device_is_unavailable() {
        let probe = FakeProbe { loads: Vec::new() };
        let sample = probe.sample(&nv("GPU-missing", 0));
        assert!(!sample.is_ready());
        assert!(sample.load().is_none());
    }

    #[test]
    fn vram_frac_narrowing_is_precise_at_quarters() {
        // Exercise the as-free f64 -> f32 narrowing at exact fractions.
        for (used, total, expect) in [
            (1_u64, 4_u64, 0.25_f32),
            (3, 4, 0.75),
            (1, 1, 1.0),
            (0, 4, 0.0),
        ] {
            let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
            load.vram_used_bytes = Some(used);
            load.vram_total_bytes = Some(total);
            let frac = load.vram_used_frac().expect("known");
            assert!(
                (frac - expect).abs() < 1e-4,
                "used={used} total={total} => {frac}, want {expect}"
            );
        }
    }

    /// Feature-gated NVIDIA probe test (ADR-0017): on a host with **no** NVIDIA
    /// device (this box / CI), `NvmlLoadProbe::try_init` must return `None`
    /// gracefully — NVML initialise-or-unavailable, never a panic. If a real
    /// NVIDIA GPU were present the probe would init and enumerate devices
    /// without panicking either; both arms are non-panicking by contract.
    #[cfg(feature = "cuda")]
    #[test]
    fn nvml_probe_is_graceful_without_a_gpu() {
        match NvmlLoadProbe::try_init() {
            None => {
                // The expected path on a non-NVIDIA host: cleanly unavailable.
            }
            Some(probe) => {
                // A real GPU is present (hardware runner): enumeration and
                // sampling must not panic, and every sample is well-formed.
                for device in probe.devices() {
                    let sample = probe.sample(&device);
                    if let Some(load) = sample.load() {
                        assert_eq!(load.device_id, device);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------------
    // Linux i915/amdgpu sysfs/fdinfo parsing (ADR-0017, ENG-4).
    //
    // These exercise the PURE string -> DeviceLoad-field parsers against
    // captured sysfs/fdinfo fixture STRINGS (no real GPU, no /sys read), plus
    // the device-tree walk against a synthetic temp directory (NOT /sys).
    // ------------------------------------------------------------------------
    #[cfg(any(feature = "vaapi", feature = "qsv"))]
    mod sysfs {
        use super::super::linux_sysfs::{
            classify_pci_vendor, parse_fdinfo_merged_media_frac, parse_gpu_busy_percent,
            parse_vram_bytes, read_device_load_from, DeviceDirs, DrmRoot, FdinfoSample,
        };
        use super::super::{LoadProbe, Vendor};

        #[test]
        fn classify_pci_vendor_maps_known_ids() {
            // Canonical lower-case hex with the kernel's `0x` prefix + trailing
            // newline, exactly as `/sys/class/drm/cardN/device/vendor` reports.
            assert_eq!(classify_pci_vendor("0x10de\n"), Some(Vendor::Nvidia));
            assert_eq!(classify_pci_vendor("0x1002\n"), Some(Vendor::Amd));
            assert_eq!(classify_pci_vendor("0x8086\n"), Some(Vendor::Intel));
            // Upper-case + no newline still classifies (robust trim/parse).
            assert_eq!(classify_pci_vendor("0x8086"), Some(Vendor::Intel));
            // An unknown / non-GPU vendor id is None, never a wrong guess.
            assert_eq!(classify_pci_vendor("0x1234\n"), None);
            assert_eq!(classify_pci_vendor("garbage"), None);
            assert_eq!(classify_pci_vendor(""), None);
        }

        #[test]
        fn gpu_busy_percent_parses_and_clamps() {
            assert_eq!(parse_gpu_busy_percent("0\n"), Some(0.0));
            assert_eq!(parse_gpu_busy_percent("100\n"), Some(1.0));
            assert_eq!(parse_gpu_busy_percent("50"), Some(0.5));
            // A driver overshoot (>100) clamps to 1.0, never exceeds the unit
            // interval and never panics.
            assert_eq!(parse_gpu_busy_percent("137\n"), Some(1.0));
            // Garbage / empty => unknown (None), never a fabricated zero.
            assert_eq!(parse_gpu_busy_percent("notanumber"), None);
            assert_eq!(parse_gpu_busy_percent(""), None);
        }

        #[test]
        fn vram_bytes_parses_decimal_byte_count() {
            // `mem_info_vram_total` / `_used` are plain decimal byte counts.
            assert_eq!(parse_vram_bytes("12884901888\n"), Some(12_884_901_888));
            assert_eq!(parse_vram_bytes("0"), Some(0));
            assert_eq!(parse_vram_bytes("  6442450944  "), Some(6_442_450_944));
            assert_eq!(parse_vram_bytes("nope"), None);
            assert_eq!(parse_vram_bytes(""), None);
        }

        #[test]
        fn fdinfo_merged_media_frac_differences_two_snapshots() {
            // DRM fdinfo reports a monotonically increasing `drm-engine-<class>`
            // nanosecond counter per client fd. The busy fraction over an
            // interval is (delta engine-ns) / (interval wall-ns). AMD VCN4+
            // merges decode+encode into one media engine figure, so we report a
            // single combined media term.
            let first =
                "drm-driver:\tamdgpu\ndrm-engine-gfx:\t1000000 ns\ndrm-engine-enc:\t2000000 ns\n";
            let second =
                "drm-driver:\tamdgpu\ndrm-engine-gfx:\t1500000 ns\ndrm-engine-enc:\t2500000 ns\n";
            let a = FdinfoSample::parse(first).expect("first parses");
            let b = FdinfoSample::parse(second).expect("second parses");
            // enc delta = 500_000 ns over a 1_000_000 ns (1 ms) wall interval =>
            // 0.5 busy fraction for the merged media engine.
            let frac = parse_fdinfo_merged_media_frac(&a, &b, 1_000_000)
                .expect("media engine fraction known");
            assert!((frac - 0.5).abs() < 1e-4, "0.5 expected, got {frac}");
        }

        #[test]
        fn fdinfo_merged_media_frac_clamps_and_guards_zero_interval() {
            let a = FdinfoSample::parse("drm-engine-dec:\t0 ns\n").expect("parses");
            let b = FdinfoSample::parse("drm-engine-dec:\t9000000 ns\n").expect("parses");
            // A zero / non-positive interval is a guard => None, never a divide.
            assert_eq!(parse_fdinfo_merged_media_frac(&a, &b, 0), None);
            // A delta exceeding the interval clamps to 1.0 (saturated engine).
            let frac = parse_fdinfo_merged_media_frac(&a, &b, 1_000_000).expect("known");
            assert_eq!(frac, 1.0);
        }

        #[test]
        fn fdinfo_with_no_engine_keys_yields_none_media() {
            let a = FdinfoSample::parse("drm-driver:\tamdgpu\n").expect("parses");
            let b = FdinfoSample::parse("drm-driver:\tamdgpu\n").expect("parses");
            // No media engine counters at all => unknown, not a fabricated zero.
            assert_eq!(parse_fdinfo_merged_media_frac(&a, &b, 1_000_000), None);
        }

        /// Build a synthetic `card0/device/` tree under a unique temp dir (NOT
        /// `/sys`) so the read path is exercised without a real GPU.
        fn write_amd_card(root: &std::path::Path, card: &str) -> std::io::Result<()> {
            let dev = root.join(card).join("device");
            std::fs::create_dir_all(&dev)?;
            std::fs::write(dev.join("vendor"), "0x1002\n")?;
            std::fs::write(dev.join("device"), "0x73bf\n")?;
            std::fs::write(dev.join("gpu_busy_percent"), "42\n")?;
            std::fs::write(dev.join("mem_info_vram_total"), "17163091968\n")?;
            std::fs::write(dev.join("mem_info_vram_used"), "4290772992\n")?;
            // A stable PCI bus id is the symlink target's basename in real sysfs;
            // the synthetic tree provides it via a `uevent` PCI_SLOT_NAME line.
            std::fs::write(
                dev.join("uevent"),
                "DRIVER=amdgpu\nPCI_SLOT_NAME=0000:03:00.0\n",
            )?;
            Ok(())
        }

        #[test]
        fn read_amd_card_from_synthetic_tree() {
            let base = std::env::temp_dir().join(format!(
                "mv-eng4-amd-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = std::fs::remove_dir_all(&base);
            write_amd_card(&base, "card0").expect("fixture tree");

            let dirs = DeviceDirs::for_card(&base, "card0").expect("classified as a GPU");
            assert_eq!(dirs.device_id().vendor(), Vendor::Amd);
            // Stable id is the PCI bus id, never the enumeration index.
            assert_eq!(dirs.device_id().stable_id(), "0000:03:00.0");

            let load = read_device_load_from(&dirs).expect("a Ready load");
            assert_eq!(load.device_id.vendor(), Vendor::Amd);
            assert_eq!(load.device_id.stable_id(), "0000:03:00.0");
            assert!(
                (load.gpu_busy_frac.expect("busy known") - 0.42).abs() < 1e-4,
                "42% busy"
            );
            assert_eq!(load.vram_total_bytes, Some(17_163_091_968));
            assert_eq!(load.vram_used_bytes, Some(4_290_772_992));
            // AMD exposes no per-engine sysfs % here, so enc/dec stay unknown
            // (honest None, never a fabricated zero) until an fdinfo source is
            // wired.
            assert!(load.enc_util_frac.is_none());
            assert!(load.dec_util_frac.is_none());
            assert!(load.nvenc_session_count.is_none());

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn read_device_load_missing_files_is_graceful() {
            // A device dir with a known vendor but NO sysfs metrics files must
            // yield a Ready load whose every metric is unknown — never a panic
            // and never a fabricated value.
            let base = std::env::temp_dir().join(format!(
                "mv-eng4-empty-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = std::fs::remove_dir_all(&base);
            let dev = base.join("card0").join("device");
            std::fs::create_dir_all(&dev).expect("dir");
            std::fs::write(dev.join("vendor"), "0x8086\n").expect("vendor");
            std::fs::write(
                dev.join("uevent"),
                "DRIVER=i915\nPCI_SLOT_NAME=0000:00:02.0\n",
            )
            .expect("uevent");

            let dirs = DeviceDirs::for_card(&base, "card0").expect("intel GPU");
            assert_eq!(dirs.device_id().vendor(), Vendor::Intel);
            let load = read_device_load_from(&dirs).expect("ready, all-unknown");
            assert!(load.gpu_busy_frac.is_none());
            assert!(load.vram_total_bytes.is_none());
            assert!(load.vram_used_bytes.is_none());

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn non_gpu_card_is_skipped() {
            // A `cardN` whose vendor id is not a known GPU silicon family is not
            // classified as a device (returns None), so it is never sampled.
            let base = std::env::temp_dir().join(format!(
                "mv-eng4-skip-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = std::fs::remove_dir_all(&base);
            let dev = base.join("card9").join("device");
            std::fs::create_dir_all(&dev).expect("dir");
            std::fs::write(dev.join("vendor"), "0xabcd\n").expect("vendor");
            assert!(DeviceDirs::for_card(&base, "card9").is_none());
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn probe_over_synthetic_root_enumerates_and_samples() {
            let base = std::env::temp_dir().join(format!(
                "mv-eng4-root-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = std::fs::remove_dir_all(&base);
            write_amd_card(&base, "card0").expect("card0");
            // A render-only node (`renderD128`) and a non-GPU card must be
            // ignored by the cardN walk.
            std::fs::create_dir_all(base.join("renderD128")).expect("render node");

            let probe = DrmRoot::at(&base).into_probe();
            let devices = probe.devices();
            assert_eq!(devices.len(), 1, "exactly one classified GPU");
            let load = probe.sample(&devices[0]);
            assert!(load.is_ready(), "the synthetic AMD card samples Ready");
            assert_eq!(
                probe.sample_all().len(),
                1,
                "sample_all returns the one Ready load"
            );

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn probe_over_absent_root_is_empty_never_panics() {
            // A root that does not exist (the off-Linux / no-DRM fallback shape)
            // enumerates nothing and samples nothing — graceful, never a panic.
            let probe = DrmRoot::at(std::path::Path::new("/nonexistent/mv-eng4/drm")).into_probe();
            assert!(probe.devices().is_empty());
            assert!(probe.sample_all().is_empty());
        }
    }

    // ------------------------------------------------------------------------
    // Live /proc/<pid>/fdinfo per-engine enc/dec walk (ADR-0017, ENG-4b).
    //
    // The PURE core: (1) extract the DRM `drm-pdev:` PCI bus id from an fdinfo
    // body so an fd is attributable to a DeviceId; (2) accumulate the merged
    // media-engine busy-ns per device across the process set into one snapshot;
    // (3) difference two snapshots a known wall interval apart into a 0..=1
    // busy fraction. The live `/proc` walk runs against a SYNTHETIC proc tree
    // (NOT the real /proc) so it is exercised with no GPU and on any OS.
    // ------------------------------------------------------------------------
    #[cfg(any(feature = "vaapi", feature = "qsv"))]
    mod fdinfo_walk {
        use super::super::linux_sysfs::{
            parse_fdinfo_pdev, DrmRoot, FdinfoMediaSnapshot, FdinfoMediaTracker, SnapshotClock,
        };
        use super::super::Vendor;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        #[test]
        fn pdev_line_extracts_the_pci_bus_id() {
            // The DRM common fdinfo schema reports the owning device as a
            // `drm-pdev:\t<domain>:<bus>:<dev>.<func>` line — the same bus id
            // the sysfs walk keys a DeviceId by, so an fd attributes to a card.
            let body = "drm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t42\n";
            assert_eq!(parse_fdinfo_pdev(body).as_deref(), Some("0000:03:00.0"));
            // No pdev line => unknown (cannot attribute), never a wrong guess.
            assert_eq!(parse_fdinfo_pdev("drm-driver:\tamdgpu\n"), None);
            // Empty value is not a usable id.
            assert_eq!(parse_fdinfo_pdev("drm-pdev:\t\n"), None);
        }

        #[test]
        fn snapshot_sums_media_ns_per_device_across_fds() {
            // Two render fds owned by our process set, both bound to the same
            // card, contribute their media-engine ns to ONE per-device sum.
            let fd_a =
                "drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t1000000 ns\ndrm-engine-dec:\t500000 ns\n";
            let fd_b = "drm-pdev:\t0000:03:00.0\ndrm-engine-dec:\t250000 ns\n";
            // A third fd on a different card stays separate.
            let fd_c = "drm-pdev:\t0000:01:00.0\ndrm-engine-enc:\t9000000 ns\n";

            let mut snap = FdinfoMediaSnapshot::default();
            snap.accumulate_fd(fd_a);
            snap.accumulate_fd(fd_b);
            snap.accumulate_fd(fd_c);

            // card 03:00.0 = 1_000_000 + 500_000 + 250_000 = 1_750_000 ns merged.
            assert_eq!(snap.media_ns("0000:03:00.0"), Some(1_750_000));
            assert_eq!(snap.media_ns("0000:01:00.0"), Some(9_000_000));
            // A device we never saw is unknown, never a fabricated zero.
            assert_eq!(snap.media_ns("0000:09:00.0"), None);
        }

        #[test]
        fn snapshot_ignores_fds_with_no_pdev_or_no_media() {
            // An fd with engine counters but no pdev cannot be attributed, and a
            // pdev with no media counters carries no media-ns — both are skipped
            // without panicking or polluting another device's sum.
            let mut snap = FdinfoMediaSnapshot::default();
            snap.accumulate_fd("drm-engine-enc:\t7777 ns\n"); // no pdev
            snap.accumulate_fd("drm-pdev:\t0000:03:00.0\ndrm-driver:\tamdgpu\n"); // no media
            assert_eq!(snap.media_ns("0000:03:00.0"), None);
        }

        #[test]
        fn tracker_differences_two_snapshots_into_a_fraction() {
            // The tracker holds the previous snapshot + the instant it was
            // captured; each poll diffs the new snapshot against it over the
            // elapsed between the two instants it is handed (the F1 divisor).
            // media delta 500_000 ns over a 1 ms interval => 0.5 busy for the
            // merged engine.
            let earlier = {
                let mut s = FdinfoMediaSnapshot::default();
                s.accumulate_fd("drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t1000000 ns\n");
                s
            };
            let later = {
                let mut s = FdinfoMediaSnapshot::default();
                s.accumulate_fd("drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t1500000 ns\n");
                s
            };
            let t0 = Instant::now();
            let t1 = t0 + Duration::from_millis(1);
            let mut tracker = FdinfoMediaTracker::default();
            // First poll has no prior snapshot => unknown (need two samples).
            assert!(tracker
                .merged_media_frac("0000:03:00.0", &earlier, t0)
                .is_none());
            let frac = tracker
                .merged_media_frac("0000:03:00.0", &later, t1)
                .expect("a second snapshot yields a fraction");
            assert!((frac - 0.5).abs() < 1e-4, "0.5 expected, got {frac}");
            // Two snapshots at the same instant is a zero-length interval — a
            // divide guard => None, never a panic. (`previous` now holds t1.)
            assert!(tracker
                .merged_media_frac("0000:03:00.0", &later, t1)
                .is_none());
        }

        #[test]
        fn tracker_counter_reset_is_unknown_not_negative() {
            // A counter that went backwards (driver/client reset) is unknown for
            // that tick — never a wrapped/negative fraction, never a panic.
            let high = {
                let mut s = FdinfoMediaSnapshot::default();
                s.accumulate_fd("drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t9000000 ns\n");
                s
            };
            let low = {
                let mut s = FdinfoMediaSnapshot::default();
                s.accumulate_fd("drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t10000 ns\n");
                s
            };
            let t0 = Instant::now();
            let t1 = t0 + Duration::from_millis(1);
            let mut tracker = FdinfoMediaTracker::default();
            assert!(tracker
                .merged_media_frac("0000:03:00.0", &high, t0)
                .is_none());
            assert!(tracker
                .merged_media_frac("0000:03:00.0", &low, t1)
                .is_none());
        }

        #[test]
        fn tracker_fraction_scales_with_the_snapshot_interval() {
            // F1 regression (pure differencing unit): the divisor is the elapsed
            // between the two SNAPSHOT instants the tracker retains, so the SAME
            // counter delta over a LONGER snapshot interval yields a
            // proportionally SMALLER fraction. A regression that divided by a
            // fixed interval would instead report the same fraction for both. The
            // end-to-end proof that the probe/poller seam feeds the
            // snapshot-to-snapshot interval into this divisor lives in
            // `sysfs_poller_folds_media_across_polls_and_exposes_base_load`.
            let base = {
                let mut s = FdinfoMediaSnapshot::default();
                s.accumulate_fd("drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t0 ns\n");
                s
            };
            let advanced = {
                let mut s = FdinfoMediaSnapshot::default();
                s.accumulate_fd("drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t500000 ns\n");
                s
            };

            // 500_000 ns of busy over a 1 ms snapshot interval => 0.5.
            let t0 = Instant::now();
            let mut fast = FdinfoMediaTracker::default();
            assert!(fast.merged_media_frac("0000:03:00.0", &base, t0).is_none());
            let quick = fast
                .merged_media_frac("0000:03:00.0", &advanced, t0 + Duration::from_millis(1))
                .expect("a second snapshot yields a fraction");
            assert!((quick - 0.5).abs() < 1e-4, "0.5 over 1 ms, got {quick}");

            // The SAME 500_000 ns of busy over a 2 ms snapshot interval => 0.25.
            let u0 = Instant::now();
            let mut slow = FdinfoMediaTracker::default();
            assert!(slow.merged_media_frac("0000:03:00.0", &base, u0).is_none());
            let gentle = slow
                .merged_media_frac("0000:03:00.0", &advanced, u0 + Duration::from_millis(2))
                .expect("a second snapshot yields a fraction");
            assert!((gentle - 0.25).abs() < 1e-4, "0.25 over 2 ms, got {gentle}");
        }

        /// Build a synthetic `<root>/<pid>/fdinfo/<fd>` tree (NOT real `/proc`)
        /// so the live walk is exercised with no GPU and on any OS.
        fn write_proc_fd(
            root: &std::path::Path,
            pid: u32,
            fd: u32,
            body: &str,
        ) -> std::io::Result<()> {
            let dir = root.join(pid.to_string()).join("fdinfo");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join(fd.to_string()), body)
        }

        #[test]
        fn walk_proc_fdinfo_sums_own_pids_per_device() {
            let base = std::env::temp_dir().join(format!(
                "mv-eng4b-walk-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = std::fs::remove_dir_all(&base);
            // Our process set is {1000, 1001}. Two fds on the same card across
            // two pids, plus a non-DRM fd that the walk skips.
            write_proc_fd(
                &base,
                1000,
                3,
                "drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t1000000 ns\n",
            )
            .expect("fd");
            write_proc_fd(
                &base,
                1001,
                4,
                "drm-pdev:\t0000:03:00.0\ndrm-engine-dec:\t500000 ns\n",
            )
            .expect("fd");
            write_proc_fd(&base, 1000, 9, "pos:\t0\nflags:\t0100002\n").expect("non-drm fd");

            let snap = FdinfoMediaSnapshot::walk_proc(&base, &[1000, 1001]);
            // 1_000_000 (enc, pid 1000) + 500_000 (dec, pid 1001) = 1_500_000.
            assert_eq!(snap.media_ns("0000:03:00.0"), Some(1_500_000));

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn walk_proc_absent_root_is_empty_never_panics() {
            // A missing proc root (off-Linux / no such pid) yields an empty
            // snapshot — graceful absence, never a panic, never a block.
            let snap = FdinfoMediaSnapshot::walk_proc(
                std::path::Path::new("/nonexistent/mv-eng4b/proc"),
                &[424_242],
            );
            assert_eq!(snap.media_ns("0000:03:00.0"), None);
        }

        /// Build a synthetic AMD `card0/device/` tree whose `uevent` PCI bus id
        /// matches the fdinfo `drm-pdev:` so the device attributes to the walk.
        fn write_amd_card(root: &std::path::Path) -> std::io::Result<()> {
            let dev = root.join("card0").join("device");
            std::fs::create_dir_all(&dev)?;
            std::fs::write(dev.join("vendor"), "0x1002\n")?;
            std::fs::write(dev.join("gpu_busy_percent"), "30\n")?;
            std::fs::write(dev.join("mem_info_vram_total"), "17163091968\n")?;
            std::fs::write(dev.join("mem_info_vram_used"), "4290772992\n")?;
            std::fs::write(
                dev.join("uevent"),
                "DRIVER=amdgpu\nPCI_SLOT_NAME=0000:03:00.0\n",
            )?;
            Ok(())
        }

        #[test]
        fn probe_folds_merged_media_into_enc_and_dec_after_two_polls() {
            // End-to-end of the ENG-4b wiring through the REAL capture seam: a
            // synthetic DRM card + a synthetic /proc tree whose fdinfo media
            // counter advances between polls, with an INJECTED snapshot clock so
            // the snapshot-to-snapshot interval is exact and deterministic. The
            // first poll has no prior snapshot (enc/dec stay None); the second
            // yields the EXACT merged media fraction (0.5 — a 500_000 ns counter
            // delta over a 1 ms injected interval) folded into BOTH enc and dec.
            // Asserting the exact non-saturated fraction through
            // `sample_all_with_media` de-saturates the fold and proves the
            // divisor is the snapshot-to-snapshot interval (not a saturated
            // clamp). It does NOT pin the within-poll capture POINT (walk vs
            // poll-entry) — the injected clock is constant within a poll, so
            // that <0.001%-impact distinction is an accepted residual.
            let drm = std::env::temp_dir().join(format!(
                "mv-eng4b-drm-{}-{}",
                std::process::id(),
                line!()
            ));
            let proc = std::env::temp_dir().join(format!(
                "mv-eng4b-proc-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = std::fs::remove_dir_all(&drm);
            let _ = std::fs::remove_dir_all(&proc);
            write_amd_card(&drm).expect("amd card");

            // Inject a controllable snapshot clock so the two walks are an exact
            // interval apart (production reads the real monotonic clock).
            let clock = Arc::new(Mutex::new(Instant::now()));
            let t0 = *clock.lock().expect("clock");
            let mut probe = DrmRoot::at(&drm)
                .into_probe()
                .with_proc_root(&proc)
                .with_clock(SnapshotClock::Manual(Arc::clone(&clock)));

            // Poll 1 at t0: media counter at 1_000_000 ns; no prior snapshot yet.
            write_proc_fd(
                &proc,
                4242,
                3,
                "drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t1000000 ns\n",
            )
            .expect("fd v1");
            let first = probe.sample_all_with_media(&[4242]);
            assert_eq!(first.len(), 1, "the one synthetic AMD card");
            assert!(
                first[0].enc_util_frac.is_none() && first[0].dec_util_frac.is_none(),
                "first poll has no prior snapshot => unknown, not a fabricated zero"
            );

            // Advance the injected clock by EXACTLY 1 ms and the counter by
            // EXACTLY 500_000 ns => 0.5 busy for the merged engine, folded into
            // BOTH enc and dec. The exact (non-saturated) fraction only holds if
            // the divisor is the injected snapshot-to-snapshot interval.
            *clock.lock().expect("clock") = t0 + Duration::from_millis(1);
            std::fs::write(
                proc.join("4242").join("fdinfo").join("3"),
                "drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t1500000 ns\n",
            )
            .expect("fd v2");
            let second = probe.sample_all_with_media(&[4242]);
            assert_eq!(second.len(), 1);
            let load = &second[0];
            assert_eq!(load.device_id.vendor(), Vendor::Amd);
            let enc = load.enc_util_frac.expect("enc folded from merged media");
            let dec = load.dec_util_frac.expect("dec folded from merged media");
            // VCN4+ merges decode+encode — both carry the same combined term.
            assert!((enc - 0.5).abs() < 1e-4, "0.5 enc over 1 ms, got {enc}");
            assert!((dec - 0.5).abs() < 1e-4, "0.5 dec over 1 ms, got {dec}");

            let _ = std::fs::remove_dir_all(&drm);
            let _ = std::fs::remove_dir_all(&proc);
        }

        #[test]
        fn probe_with_no_matching_fds_leaves_enc_dec_unknown() {
            // A card whose PCI bus id no fdinfo fd references gets no media term —
            // enc/dec stay None (honest unknown), never a fabricated zero, even
            // across two polls.
            let drm = std::env::temp_dir().join(format!(
                "mv-eng4b-drm-nomatch-{}-{}",
                std::process::id(),
                line!()
            ));
            let proc = std::env::temp_dir().join(format!(
                "mv-eng4b-proc-nomatch-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = std::fs::remove_dir_all(&drm);
            let _ = std::fs::remove_dir_all(&proc);
            write_amd_card(&drm).expect("amd card");
            // An fd for a DIFFERENT device than the card.
            write_proc_fd(
                &proc,
                4243,
                3,
                "drm-pdev:\t0000:09:00.0\ndrm-engine-enc:\t1000000 ns\n",
            )
            .expect("fd");

            let mut probe = DrmRoot::at(&drm).into_probe().with_proc_root(&proc);
            let _ = probe.sample_all_with_media(&[4243]);
            std::fs::write(
                proc.join("4243").join("fdinfo").join("3"),
                "drm-pdev:\t0000:09:00.0\ndrm-engine-enc:\t1500000 ns\n",
            )
            .expect("fd v2");
            let loads = probe.sample_all_with_media(&[4243]);
            assert_eq!(loads.len(), 1);
            assert!(
                loads[0].enc_util_frac.is_none() && loads[0].dec_util_frac.is_none(),
                "no fd matches the card's bus id => unknown enc/dec"
            );

            let _ = std::fs::remove_dir_all(&drm);
            let _ = std::fs::remove_dir_all(&proc);
        }

        #[test]
        fn sysfs_poller_folds_media_across_polls_and_exposes_base_load() {
            // The rule-6 wire: `SysfsLoadPoller` drives `SysfsLoadProbe`'s
            // stateful two-snapshot media path through the real `LoadSource::poll`
            // seam, retaining the prior snapshot + its capture instant across
            // polls behind interior mutability. The base sysfs load (gpu-busy,
            // VRAM) flows on every poll; the merged media term appears once a
            // prior snapshot exists. An INJECTED snapshot clock makes the
            // snapshot-to-snapshot interval exact, so this asserts the EXACT
            // merged fraction end-to-end through `LoadSource::poll` (0.5 over a
            // 1 ms interval, then 0.25 for the SAME counter delta over 2 ms —
            // proving the divisor tracks the injected snapshot-to-snapshot
            // interval, not a fixed constant, and never a saturated clamp).
            use super::super::{LoadSource, SysfsLoadPoller};

            let drm = std::env::temp_dir().join(format!(
                "mv-eng4b-poller-drm-{}-{}",
                std::process::id(),
                line!()
            ));
            let proc = std::env::temp_dir().join(format!(
                "mv-eng4b-poller-proc-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = std::fs::remove_dir_all(&drm);
            let _ = std::fs::remove_dir_all(&proc);
            write_amd_card(&drm).expect("amd card");
            write_proc_fd(
                &proc,
                4242,
                3,
                "drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t1000000 ns\n",
            )
            .expect("fd v1");

            let clock = Arc::new(Mutex::new(Instant::now()));
            let t0 = *clock.lock().expect("clock");
            let probe = DrmRoot::at(&drm)
                .into_probe()
                .with_proc_root(&proc)
                .with_clock(SnapshotClock::Manual(Arc::clone(&clock)));
            let poller = SysfsLoadPoller::from_probe(probe, vec![4242]);

            // Poll 1: base load present, media unknown (no prior snapshot yet —
            // honest None, never a fabricated zero).
            let first = LoadSource::poll(&poller);
            assert_eq!(first.len(), 1, "the one synthetic AMD card");
            assert!(
                (first[0].gpu_busy_frac.expect("busy known") - 0.30).abs() < 1e-4,
                "base sysfs gpu-busy flows through the poller"
            );
            assert_eq!(first[0].vram_total_bytes, Some(17_163_091_968));
            assert!(
                first[0].enc_util_frac.is_none() && first[0].dec_util_frac.is_none(),
                "first poll has no prior media snapshot"
            );

            // Poll 2 at t0 + 1 ms with the counter advanced by EXACTLY 500_000 ns
            // => EXACTLY 0.5, folded into BOTH enc and dec (VCN4+ merges the media
            // engines) through the real LoadSource::poll seam — no saturation.
            *clock.lock().expect("clock") = t0 + Duration::from_millis(1);
            std::fs::write(
                proc.join("4242").join("fdinfo").join("3"),
                "drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t1500000 ns\n",
            )
            .expect("fd v2");
            let second = LoadSource::poll(&poller);
            assert_eq!(second.len(), 1);
            let load = &second[0];
            let enc = load.enc_util_frac.expect("enc folded from merged media");
            let dec = load.dec_util_frac.expect("dec folded from merged media");
            assert!((enc - 0.5).abs() < 1e-4, "0.5 enc over 1 ms, got {enc}");
            assert!((dec - 0.5).abs() < 1e-4, "0.5 dec over 1 ms, got {dec}");

            // Poll 3: the SAME 500_000 ns counter delta over a 2 ms injected
            // interval => 0.25 (proportionally smaller). Proves the divisor at
            // the poll seam IS the injected snapshot-to-snapshot interval — a
            // fixed divisor would report 0.5 again.
            *clock.lock().expect("clock") = t0 + Duration::from_millis(3);
            std::fs::write(
                proc.join("4242").join("fdinfo").join("3"),
                "drm-pdev:\t0000:03:00.0\ndrm-engine-enc:\t2000000 ns\n",
            )
            .expect("fd v3");
            let third = LoadSource::poll(&poller);
            assert_eq!(third.len(), 1);
            let enc3 = third[0]
                .enc_util_frac
                .expect("enc folded from merged media");
            let dec3 = third[0]
                .dec_util_frac
                .expect("dec folded from merged media");
            assert!((enc3 - 0.25).abs() < 1e-4, "0.25 enc over 2 ms, got {enc3}");
            assert!((dec3 - 0.25).abs() < 1e-4, "0.25 dec over 2 ms, got {dec3}");

            let _ = std::fs::remove_dir_all(&drm);
            let _ = std::fs::remove_dir_all(&proc);
        }

        #[test]
        fn sysfs_poller_poll_trait_yields_base_load_and_is_send_sync() {
            // The `LoadSource::poll(&self)` seam (the one `default_load_source`
            // hands the metrics task) yields the base sysfs load on a
            // live-interval poll; and the poller is `Send + Sync`, so it fits the
            // `Box<dyn LoadSource + Send + Sync>` the task stores.
            use super::super::{LoadSource, SysfsLoadPoller};

            fn assert_send_sync<T: Send + Sync>() {}
            assert_send_sync::<SysfsLoadPoller>();

            let drm = std::env::temp_dir().join(format!(
                "mv-eng4b-poller-trait-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = std::fs::remove_dir_all(&drm);
            write_amd_card(&drm).expect("amd card");

            let probe = DrmRoot::at(&drm).into_probe();
            let poller = SysfsLoadPoller::from_probe(probe, vec![std::process::id()]);
            let loads: Vec<_> = LoadSource::poll(&poller);
            assert_eq!(loads.len(), 1, "the one synthetic AMD card via the trait");
            assert!(
                (loads[0].gpu_busy_frac.expect("busy") - 0.30).abs() < 1e-4,
                "base sysfs load via LoadSource::poll"
            );

            let _ = std::fs::remove_dir_all(&drm);
        }
    }

    // ------------------------------------------------------------------------
    // Intel i915 PMU media-util fallback (ADR-0017, ENG-4c).
    //
    // The live `perf_event_open` read needs a real i915 + perf permission and is
    // gated in the FFI leaf crate's own tests. Here we exercise the PURE fold
    // decision (PMU fills enc/dec only where fdinfo left them unknown, Intel
    // only) and the tracker's no-counter / non-Intel = None contract — no real
    // PMU, no syscall.
    // ------------------------------------------------------------------------
    #[cfg(feature = "i915-pmu")]
    mod i915_pmu {
        use super::super::{
            fold_intel_pmu_media_frac, DeviceId, DeviceLoad, IntelPmuMediaTracker, Vendor,
        };

        fn intel(id: &str) -> DeviceId {
            DeviceId::new(Vendor::Intel, id, 0)
        }

        #[test]
        fn pmu_frac_fills_only_unknown_enc_dec_on_intel() {
            // An Intel device whose fdinfo walk left enc/dec unknown: the PMU
            // fraction fills BOTH (Intel meters VCS+VECS; we report one merged
            // media term, mirroring the AMD fallback).
            let mut load = DeviceLoad::unknown(intel("0000:00:02.0"));
            fold_intel_pmu_media_frac(&mut load, Some(0.5));
            assert_eq!(load.enc_util_frac, Some(0.5));
            assert_eq!(load.dec_util_frac, Some(0.5));
        }

        #[test]
        fn pmu_frac_never_overwrites_an_fdinfo_reading() {
            // fdinfo already supplied a per-process enc reading; the PMU is the
            // fallback and must NOT clobber it. The unknown dec field is filled.
            let mut load = DeviceLoad::unknown(intel("0000:00:02.0"));
            load.enc_util_frac = Some(0.2);
            fold_intel_pmu_media_frac(&mut load, Some(0.9));
            assert_eq!(
                load.enc_util_frac,
                Some(0.2),
                "fdinfo enc reading preserved"
            );
            assert_eq!(load.dec_util_frac, Some(0.9), "unknown dec filled by PMU");
        }

        #[test]
        fn pmu_frac_is_a_noop_for_non_intel_devices() {
            // The i915 PMU is Intel-only; an AMD device must never be touched by
            // it even if a fraction is offered.
            let mut load = DeviceLoad::unknown(DeviceId::new(Vendor::Amd, "0000:03:00.0", 0));
            fold_intel_pmu_media_frac(&mut load, Some(0.7));
            assert!(load.enc_util_frac.is_none(), "AMD enc untouched");
            assert!(load.dec_util_frac.is_none(), "AMD dec untouched");
        }

        #[test]
        fn pmu_none_fraction_is_a_noop() {
            // No counter / first poll / denied => None fraction: the fields stay
            // honest-unknown, never a fabricated zero.
            let mut load = DeviceLoad::unknown(intel("0000:00:02.0"));
            fold_intel_pmu_media_frac(&mut load, None);
            assert!(load.enc_util_frac.is_none());
            assert!(load.dec_util_frac.is_none());
        }

        #[test]
        fn tracker_is_none_for_non_intel_and_without_a_counter() {
            // A non-Intel device is never sampled by the PMU tracker. An Intel
            // device with no real i915 PMU (this box / CI) cannot open a counter,
            // so the first (and every) poll is the honest None — never a panic,
            // never a block.
            let mut tracker = IntelPmuMediaTracker::default();
            assert_eq!(
                tracker.merged_media_frac(&DeviceId::new(Vendor::Amd, "0000:03:00.0", 0)),
                None,
                "non-Intel device is not a PMU candidate"
            );
            assert_eq!(
                tracker.merged_media_frac(&intel("0000:00:02.0")),
                None,
                "no i915 PMU here => unknown, never a panic"
            );
        }
    }
}
