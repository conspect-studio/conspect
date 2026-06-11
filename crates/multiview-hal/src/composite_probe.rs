//! The SA-0 composite-usability probe + the both-halves MISMATCH cross-check.
//!
//! This is the smallest slice of self-aware placement (ADR-0035): detect — at
//! pipeline **build time**, off the output-clock thread — when a real GPU is
//! present on the host yet the wgpu compositor resolved a **software / CPU-tier**
//! adapter (the verified silent fallback that burned ~5 CPU cores while the GPU
//! sat idle, because the container lacked the Vulkan loader / the `graphics`
//! driver capability).
//!
//! The module is split into two pure pieces so the load-bearing logic is
//! exhaustively unit-testable with **no** GPU and **no** wgpu types:
//!
//! 1. [`AdapterClass`] — the classification of the compositor's resolved adapter
//!    into `Gpu` (a real accelerator) vs `Software` (llvmpipe / lavapipe /
//!    swiftshader / `device_type == Cpu`) vs `None` (no adapter at all). The
//!    build site maps `wgpu::AdapterInfo` into an [`AdapterReport`] (a plain,
//!    wgpu-free description) and this module classifies it — so the *load-bearing
//!    discriminator* (`device_type != Cpu`, plus the driver-string guard, never
//!    a blanket `Gl` exclusion) is tested here, not behind a feature flag.
//! 2. [`hardware_present`] + [`composite_mismatch`] — the **both-halves
//!    cross-check**: a warning fires **only** when hardware is *discovered
//!    present* (NVML/`DeviceLoad` reports ≥1 GPU **OR** an `EnvProbe`/presence
//!    probe says Present, independently of which feature is compiled) **AND** the
//!    composite resolved software/CPU-tier. On a GPU-free host, or an intentional
//!    software-only host, **neither** half is true → **zero** warnings (the
//!    no-false-positive rule).
//!
//! This module computes a structured [`CompositeMismatch`] description; the WARN
//! model (`HealthWarning`, the event variants, the REST surface) lives in
//! `multiview-events` / `multiview-control`, and the emission is wired at the
//! build site (the CLI) which already owns the `GpuContext`. The warning is a
//! **latched** build-time fact (raised once, cleared on reconfigure/restart) — it
//! cannot flap.

use crate::load::DeviceLoad;

/// The device-type tier the compositor's resolved wgpu adapter reports.
///
/// A wgpu-free mirror of the relevant `wgpu::DeviceType` discriminants. The build
/// site (which owns the `wgpu` types) maps `adapter().get_info().device_type` into
/// this so the classification logic stays pure and testable without a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdapterDeviceType {
    /// A discrete GPU (the ideal compositing target).
    DiscreteGpu,
    /// An integrated GPU.
    IntegratedGpu,
    /// A virtual GPU (e.g. a paravirtualised adapter) — warn at LOWER severity.
    VirtualGpu,
    /// A pure-CPU adapter (Mesa llvmpipe/lavapipe reports this even though its
    /// backend is `Vulkan`). The **load-bearing software discriminator**.
    Cpu,
    /// A device wgpu could not classify — warn at LOWER severity.
    Other,
}

/// A plain, wgpu-free description of the compositor's resolved adapter.
///
/// The build site fills this from `wgpu::Adapter::get_info()` (the unused lever —
/// `device_type`, `driver`, and the backend's "is-GL" bit). Keeping it a plain
/// struct means [`AdapterClass::classify`] — the part that decides "real GPU vs
/// software" — is unit-tested with no GPU and no `wgpu` dependency in this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterReport {
    /// The adapter's device type (`device_type != Cpu` is the discriminator).
    pub device_type: AdapterDeviceType,
    /// The driver string (`driver` from `AdapterInfo`); a `llvmpipe` / `softpipe`
    /// / `swiftshader` match marks a software adapter even when the device type is
    /// not `Cpu`.
    pub driver: String,
    /// Whether the resolved backend is the GL backend. **Not** a software signal
    /// on its own — a legitimate GL-only real GPU reports `Discrete`/`Integrated`
    /// and must not be a false negative — so it only marks software when paired
    /// with `device_type == Cpu` or a software driver string.
    pub is_gl_backend: bool,
}

impl AdapterReport {
    /// Whether this adapter's driver string names a known software rasteriser.
    ///
    /// Matches `llvmpipe`, `lavapipe`, `softpipe`, and `swiftshader`
    /// case-insensitively (the Mesa / ANGLE software backends). This is the
    /// secondary guard so a `Gl`/`Vulkan` adapter whose `device_type` is *not*
    /// reported as `Cpu` (some drivers misreport) is still caught as software.
    #[must_use]
    pub fn driver_is_software(&self) -> bool {
        let driver = self.driver.to_ascii_lowercase();
        ["llvmpipe", "lavapipe", "softpipe", "swiftshader"]
            .iter()
            .any(|needle| driver.contains(needle))
    }
}

/// The classification of the compositor's resolved adapter for usability.
///
/// This is the first half of the cross-check ("did composite resolve software /
/// CPU-tier?"). The `device_type != Cpu` rule is the load-bearing discriminator;
/// `{VirtualGpu, Other}` are usable GPUs but warrant a **lower-severity** heads-up
/// (they can only *under*-warn, never over-warn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdapterClass {
    /// A real, usable GPU (`Discrete`/`Integrated`, not a software rasteriser).
    Gpu,
    /// A real but lower-confidence GPU (`VirtualGpu`/`Other`) — usable, but
    /// surfaced at lower severity so a paravirtualised/unknown adapter is noted.
    LowConfidenceGpu,
    /// A software / CPU-tier adapter (llvmpipe/lavapipe/swiftshader, or
    /// `device_type == Cpu`). Compositing on this is the silent-fallback trap.
    Software,
    /// No adapter at all (the compositor got `Err`, e.g. no Vulkan loader). This
    /// is the canonical no-Vulkan-adapter case — software-tier for the warning.
    None,
}

impl AdapterClass {
    /// Classify the compositor's resolved adapter.
    ///
    /// `report` is `Some` when the compositor obtained an adapter (mapped from
    /// `adapter().get_info()`), `None` when adapter acquisition failed entirely
    /// (no usable Vulkan adapter — the exact verified failure).
    ///
    /// Rules (ADR-0035 §4):
    /// * no adapter → [`AdapterClass::None`];
    /// * `device_type == Cpu`, **or** a software driver string → [`AdapterClass::Software`];
    /// * `{VirtualGpu, Other}` (not software-driver) → [`AdapterClass::LowConfidenceGpu`];
    /// * `{DiscreteGpu, IntegratedGpu}` not flagged software → [`AdapterClass::Gpu`].
    #[must_use]
    pub fn classify(report: Option<&AdapterReport>) -> Self {
        let Some(report) = report else {
            return Self::None;
        };
        // A software driver string outs an adapter as software regardless of the
        // device type it claims (swiftshader can report a non-Cpu type).
        if report.driver_is_software() {
            return Self::Software;
        }
        match report.device_type {
            // The load-bearing discriminator: a CPU device type is software even
            // when the backend reports Vulkan (Mesa llvmpipe/lavapipe).
            AdapterDeviceType::Cpu => Self::Software,
            AdapterDeviceType::DiscreteGpu | AdapterDeviceType::IntegratedGpu => Self::Gpu,
            // Virtual / unclassified adapters are usable but lower confidence. A
            // bare GL backend is NOT, on its own, a software signal here (a real
            // GL-only GPU reports Discrete/Integrated above).
            AdapterDeviceType::VirtualGpu | AdapterDeviceType::Other => Self::LowConfidenceGpu,
        }
    }

    /// Whether this class is a software / CPU-tier resolution (the second half of
    /// the cross-check). [`AdapterClass::None`] counts as software for the warning
    /// (no usable adapter ⇒ the compositor fell back to CPU).
    #[must_use]
    pub const fn is_software_tier(self) -> bool {
        matches!(self, Self::Software | Self::None)
    }
}

/// Whether hardware is **discovered present** — the FIRST half of the cross-check.
///
/// A GPU is "present" if the live load snapshot reports **≥1 GPU**
/// (NVML/`DeviceLoad`) **OR** a presence probe (`EnvProbe`/DRM) says Present —
/// `presence_probe_present` is supplied independently of which feature is
/// compiled, so a GPU box running an *asymmetric* build (neither NVML nor wgpu
/// linked) still trips the detector (the ADR-0035 blind-spot guard).
#[must_use]
pub fn hardware_present(loads: &[DeviceLoad], presence_probe_present: bool) -> bool {
    !loads.is_empty() || presence_probe_present
}

/// A structured description of a detected composite capability MISMATCH.
///
/// Produced by [`composite_mismatch`] **only** when both halves are true. Carries
/// the resolved [`class`](CompositeMismatch::class) and the discovered GPU's name
/// (if any, from the first `DeviceLoad`) so the build site can compose a clear,
/// actionable health warning (the `graphics` / `libvulkan1` remediation lives
/// with the warning catalog, not here).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompositeMismatch {
    /// How the compositor's adapter classified (`Software` or `None`).
    pub class: AdapterClass,
    /// The discovered GPU's stable id, if a `DeviceLoad` named one (else `None`,
    /// e.g. an asymmetric build where only the presence probe tripped).
    pub device_id: Option<String>,
    /// The vendor of the discovered GPU, where a `DeviceLoad` named one.
    pub vendor: Option<crate::load::Vendor>,
}

/// The both-halves cross-check: did a discovered GPU resolve a software composite?
///
/// Returns `Some(CompositeMismatch)` **only** when *both*:
/// 1. hardware is discovered present ([`hardware_present`]), **and**
/// 2. the composite adapter resolved software/CPU-tier
///    ([`AdapterClass::is_software_tier`]).
///
/// On a GPU-free host (`loads` empty, presence probe `false`) **or** an
/// intentional software-only host that nonetheless got a usable adapter, this
/// returns `None` — the no-false-positive rule, verified structurally rather than
/// by a config flag. `{Gpu, LowConfidenceGpu}` adapters are usable, so they never
/// produce a mismatch here (the low-confidence heads-up is surfaced by the build
/// site at lower severity, independently of this gate).
#[must_use]
pub fn composite_mismatch(
    loads: &[DeviceLoad],
    presence_probe_present: bool,
    class: AdapterClass,
) -> Option<CompositeMismatch> {
    if !hardware_present(loads, presence_probe_present) {
        return None;
    }
    if !class.is_software_tier() {
        return None;
    }
    let first = loads.first();
    Some(CompositeMismatch {
        class,
        device_id: first.map(|load| load.device_id.stable_id().to_owned()),
        vendor: first.map(|load| load.device_id.vendor()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::load::{DeviceId, DeviceLoad, Vendor};

    fn gpu_load() -> DeviceLoad {
        DeviceLoad::unknown(DeviceId::new(Vendor::Nvidia, "GPU-rtx4060-uuid", 0))
    }

    fn report(device_type: AdapterDeviceType, driver: &str, is_gl: bool) -> AdapterReport {
        AdapterReport {
            device_type,
            driver: driver.to_owned(),
            is_gl_backend: is_gl,
        }
    }

    // ---- classification: device_type != Cpu is the discriminator ----

    #[test]
    fn discrete_real_gpu_classifies_as_gpu() {
        let r = report(AdapterDeviceType::DiscreteGpu, "NVIDIA 550.0", false);
        assert_eq!(AdapterClass::classify(Some(&r)), AdapterClass::Gpu);
    }

    #[test]
    fn llvmpipe_cpu_device_type_classifies_as_software() {
        // Mesa llvmpipe reports device_type == Cpu even though backend == Vulkan.
        let r = report(AdapterDeviceType::Cpu, "llvmpipe (LLVM 17)", false);
        assert_eq!(AdapterClass::classify(Some(&r)), AdapterClass::Software);
    }

    #[test]
    fn gl_only_real_gpu_is_not_a_false_negative() {
        // A legitimate GL-only real GPU reports Discrete/Integrated; the Gl
        // backend must NOT, on its own, make it software (ADR-0035 false-negative
        // guard).
        let r = report(AdapterDeviceType::IntegratedGpu, "Mesa Intel(R)", true);
        assert_eq!(AdapterClass::classify(Some(&r)), AdapterClass::Gpu);
    }

    #[test]
    fn software_driver_string_on_a_gpu_device_type_is_still_software() {
        // swiftshader can report a non-Cpu device type; the driver string outs it.
        let r = report(AdapterDeviceType::Other, "SwiftShader Device", true);
        assert_eq!(AdapterClass::classify(Some(&r)), AdapterClass::Software);
    }

    #[test]
    fn virtual_and_other_are_low_confidence_not_software() {
        let v = report(AdapterDeviceType::VirtualGpu, "virtio-gpu", false);
        assert_eq!(
            AdapterClass::classify(Some(&v)),
            AdapterClass::LowConfidenceGpu
        );
        let o = report(AdapterDeviceType::Other, "unknown", false);
        assert_eq!(
            AdapterClass::classify(Some(&o)),
            AdapterClass::LowConfidenceGpu
        );
    }

    #[test]
    fn no_adapter_classifies_as_none_and_is_software_tier() {
        assert_eq!(AdapterClass::classify(None), AdapterClass::None);
        assert!(AdapterClass::None.is_software_tier());
        assert!(AdapterClass::Software.is_software_tier());
        assert!(!AdapterClass::Gpu.is_software_tier());
        assert!(!AdapterClass::LowConfidenceGpu.is_software_tier());
    }

    // ---- THE both-halves cross-check truth table (the test that matters most) --

    #[test]
    fn gpu_present_and_composite_cpu_warns() {
        // {GPU present × composite CPU} = WARN.
        let loads = [gpu_load()];
        let mismatch = composite_mismatch(&loads, false, AdapterClass::Software);
        let mismatch = mismatch.expect("a discovered GPU on a software composite must WARN");
        assert_eq!(mismatch.class, AdapterClass::Software);
        assert_eq!(mismatch.device_id.as_deref(), Some("GPU-rtx4060-uuid"));
        assert_eq!(mismatch.vendor, Some(Vendor::Nvidia));
    }

    #[test]
    fn gpu_present_via_presence_probe_only_still_warns() {
        // The asymmetric-build blind-spot guard: no DeviceLoad (NVML not linked),
        // but the EnvProbe/DRM presence probe says Present → still WARN. No id.
        let mismatch = composite_mismatch(&[], true, AdapterClass::None);
        let mismatch = mismatch.expect("presence-probe-only discovery must still WARN");
        assert_eq!(mismatch.class, AdapterClass::None);
        assert_eq!(mismatch.device_id, None);
        assert_eq!(mismatch.vendor, None);
    }

    #[test]
    fn gpu_present_and_composite_gpu_is_clean() {
        // {GPU present × composite GPU} = clean.
        let loads = [gpu_load()];
        assert_eq!(
            composite_mismatch(&loads, true, AdapterClass::Gpu),
            None,
            "a working GPU composite must NOT warn"
        );
        // A low-confidence GPU is usable: no mismatch here (the lower-severity
        // heads-up is surfaced separately, not by this gate).
        assert_eq!(
            composite_mismatch(&loads, true, AdapterClass::LowConfidenceGpu),
            None
        );
    }

    #[test]
    fn no_gpu_and_composite_cpu_is_clean_no_false_positive() {
        // {no GPU × composite CPU} = clean — the load-bearing no-false-positive
        // case: a GPU-free / intentional software-only host trips NEITHER half.
        assert_eq!(
            composite_mismatch(&[], false, AdapterClass::Software),
            None,
            "a GPU-free host on CPU composite must NEVER warn"
        );
        assert_eq!(
            composite_mismatch(&[], false, AdapterClass::None),
            None,
            "no adapter on a GPU-free host must NEVER warn"
        );
    }

    #[test]
    fn no_gpu_and_composite_gpu_is_clean() {
        // {no GPU × composite GPU} = n/a, and certainly clean.
        assert_eq!(composite_mismatch(&[], false, AdapterClass::Gpu), None);
    }

    #[test]
    fn hardware_present_ors_loads_and_presence_probe() {
        assert!(!hardware_present(&[], false));
        assert!(hardware_present(&[], true));
        assert!(hardware_present(&[gpu_load()], false));
        assert!(hardware_present(&[gpu_load()], true));
    }
}
