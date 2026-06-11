//! Build-time capability cross-check + warning emit (SA-0 / ADR-0035).
//!
//! This is the **thin build-site seam** that wires the (pure, tested) hal
//! cross-check [`multiview_hal::composite_mismatch`] to the (pure, tested)
//! control emit helper [`multiview_control::emit_capability_warnings`]. It runs at
//! pipeline **build time**, off the output-clock thread (the clock is not yet
//! constructed → inv #1 preserved), and emits only through the engine's
//! drop-oldest publisher (inv #10).
//!
//! The actual policy lives in the libraries; this module only:
//! 1. discovers hardware presence (a `DeviceLoad` snapshot from the existing load
//!    source **OR** the `EnvProbe`/DRM presence probe — independently of which
//!    feature is compiled, the ADR-0035 blind-spot guard);
//! 2. (under the `gpu` feature) reads the wgpu compositor adapter's
//!    `adapter().get_info()` — the previously-unused lever — into a wgpu-free
//!    [`multiview_hal::AdapterReport`] and classifies it;
//! 3. runs the both-halves cross-check and, on a MISMATCH, emits the latched
//!    `gpu-present-no-vulkan-adapter` warning.
//!
//! The pure mapping ([`composite_mismatch_view`], [`env_probe_present`]) is unit
//! tested here; the wgpu read is exercised on GPU hardware (and is a no-op on the
//! default GPU-free build, where the CPU composite is the *intentional* choice and
//! nothing is emitted — the no-false-positive rule).

use multiview_control::CompositeMismatchView;
use multiview_hal::composite_probe::CompositeMismatch;

/// Map a hal [`CompositeMismatch`] into the control-plane [`CompositeMismatchView`]
/// the emit helper phrases the warning from.
///
/// Pure + dependency-light: it carries the discovered GPU's name (if any) and
/// whether the compositor got **no** adapter at all (vs a software adapter), which
/// only changes the wording, not the catalog code.
#[must_use]
pub fn composite_mismatch_view(mismatch: &CompositeMismatch) -> CompositeMismatchView {
    use multiview_hal::AdapterClass;
    CompositeMismatchView {
        // SA-0 carries the stable id as the best available name; a richer
        // human-readable device name lands with the full CapabilityReport (SA-1+).
        gpu_name: mismatch.device_id.clone(),
        no_adapter: matches!(mismatch.class, AdapterClass::None),
    }
}

/// Whether the host's `EnvProbe` (device-node / DRM presence) reports any
/// accelerator present — the feature-independent half of the "hardware
/// discovered" cross-check (the ADR-0035 blind-spot guard).
///
/// Returns `true` if any probeable hardware kind resolves to
/// [`ProbeOutcome::Present`](multiview_hal::ProbeOutcome::Present). On the default
/// GPU-free build (every hardware feature off) every kind is `Absent`, so this is
/// `false` — and the cross-check's first half then rests solely on a live
/// `DeviceLoad` snapshot.
#[must_use]
pub fn env_probe_present() -> bool {
    use multiview_hal::probe::DeviceProbe;
    use multiview_hal::{EnvProbe, HardwareKind, ProbeOutcome, Stage};
    let probe = EnvProbe::new();
    HardwareKind::ALL.iter().any(|&kind| {
        matches!(
            probe.detect(kind, Stage::Composite),
            ProbeOutcome::Present(_)
        )
    })
}

/// Read the wgpu compositor adapter into a wgpu-free [`AdapterReport`], or `None`
/// when no adapter could be acquired (the canonical no-Vulkan-adapter case).
///
/// This is the `adapter().get_info()` lever (ADR-0035 §4): it builds a throwaway
/// [`GpuContext`](multiview_compositor::GpuContext) with
/// `force_fallback_adapter: false` (so a software adapter is not silently
/// substituted) and maps `device_type` / `driver` / `backend` into the report the
/// pure classifier consumes. Off-thread, build-time only.
#[cfg(feature = "gpu")]
#[must_use]
pub fn read_compositor_adapter() -> Option<multiview_hal::AdapterReport> {
    use multiview_hal::{AdapterDeviceType, AdapterReport};

    // Acquiring the context fails (typed Err, never a panic) when there is no
    // usable adapter — exactly the silent-fallback trigger; map that to `None`.
    // The capability probe wants the DEFAULT (`HighPerformance`) adapter — it is
    // asking "what would the compositor resolve to on this host?", not pinning a
    // chosen device — so it passes `None` (the load-aware pick is a separate seam
    // at pipeline build).
    let ctx = multiview_compositor::GpuContext::new(None).ok()?;
    let info = ctx.adapter().get_info();
    let device_type = match info.device_type {
        wgpu::DeviceType::DiscreteGpu => AdapterDeviceType::DiscreteGpu,
        wgpu::DeviceType::IntegratedGpu => AdapterDeviceType::IntegratedGpu,
        wgpu::DeviceType::VirtualGpu => AdapterDeviceType::VirtualGpu,
        wgpu::DeviceType::Cpu => AdapterDeviceType::Cpu,
        wgpu::DeviceType::Other => AdapterDeviceType::Other,
    };
    Some(AdapterReport {
        device_type,
        driver: info.driver,
        is_gl_backend: info.backend == wgpu::Backend::Gl,
    })
}

/// Probe the GPU composite path and emit the SA-0 warning on a MISMATCH (the
/// build-site call site; `gpu` feature only).
///
/// Runs the both-halves cross-check at build time, off the output-clock thread
/// (the clock is not yet constructed → inv #1), and emits any warning through the
/// engine's drop-oldest publisher (inv #10). On a clean / GPU-free / intentional
/// software-only host it emits nothing. Returns the number of warnings published.
///
/// `load_source` is the **existing** system-metrics load source (no second
/// poller); one bounded `poll()` snapshot supplies the "GPU discovered" half. The
/// `EnvProbe` presence ORs in independently so an asymmetric build still trips.
#[cfg(feature = "gpu")]
#[must_use]
pub fn probe_and_emit<S>(
    publisher: &multiview_engine::EnginePublisher<S, multiview_events::Event>,
    load_source: &dyn multiview_hal::LoadSource,
    since_nanos: i64,
) -> usize {
    let loads = load_source.poll();
    let report = read_compositor_adapter();
    let class = multiview_hal::AdapterClass::classify(report.as_ref());
    let mismatch = multiview_hal::composite_mismatch(&loads, env_probe_present(), class).map(|m| {
        // Map the hal mismatch to the control view the emit helper phrases from.
        composite_mismatch_view(&m)
    });
    multiview_control::emit_capability_warnings(publisher, mismatch, since_nanos)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use multiview_control::CompositeMismatchView;
    use multiview_hal::{composite_mismatch, AdapterClass, DeviceId, DeviceLoad, Vendor};

    #[test]
    fn view_carries_name_and_none_flag() {
        // Build the mismatch via the public hal cross-check (the type is
        // `#[non_exhaustive]`): a discovered GPU with NO adapter at all.
        let loads = [DeviceLoad::unknown(DeviceId::new(
            Vendor::Nvidia,
            "GPU-rtx4060",
            0,
        ))];
        let m = composite_mismatch(&loads, false, AdapterClass::None)
            .expect("a discovered GPU + no adapter is a mismatch");
        assert_eq!(
            composite_mismatch_view(&m),
            CompositeMismatchView {
                gpu_name: Some("GPU-rtx4060".to_owned()),
                no_adapter: true,
            }
        );
    }

    #[test]
    fn view_flags_software_adapter_as_not_no_adapter() {
        // A presence-probe-only discovery (no DeviceLoad) on a SOFTWARE adapter.
        let m = composite_mismatch(&[], true, AdapterClass::Software)
            .expect("presence-probe discovery + software adapter is a mismatch");
        let view = composite_mismatch_view(&m);
        assert_eq!(view.gpu_name, None);
        assert!(!view.no_adapter, "a software adapter is not `no_adapter`");
    }

    #[test]
    fn env_probe_is_absent_on_the_gpu_free_default_build() {
        // The default build compiles no hardware feature, so the EnvProbe reports
        // Absent for every kind — the first cross-check half then rests on the
        // live DeviceLoad snapshot alone (no false positive on a GPU-free host).
        // The cli exposes hardware probing only via `cuda` / `apple` /
        // `linux-vaapi` (and the `nvidia`/`full` bundles), so gate on those.
        #[cfg(not(any(
            feature = "cuda",
            feature = "apple",
            feature = "linux-vaapi",
            feature = "nvidia",
            feature = "full"
        )))]
        assert!(!env_probe_present());
    }
}
