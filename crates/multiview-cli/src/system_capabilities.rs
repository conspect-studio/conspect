//! Map the running binary's HAL probe + compiled Cargo features + resolved
//! compositor adapter onto the [`multiview_control::system::SystemCapabilities`]
//! DTO served by `GET /api/v1/system/capabilities` (ADR-W030).
//!
//! This lives in the `multiview` binary — not in `multiview-control` — because
//! only the binary depends on `multiview-hal` and knows its own compiled
//! features. The result is a **static startup snapshot** installed via
//! [`multiview_control::AppState::with_capabilities`]; it never opens an engine
//! channel (invariant #10).

use multiview_control::system::{
    BackendCapability, BackendKind, BackendResolution, BuildInfo, CompositorCapability,
    CompositorClass, CompositorDeviceType, NdiAttribution, PipelineStage, SystemCapabilities,
};
use multiview_hal::{AdapterClass, AdapterReport, BackendKind as HalKind, Stage};

/// Resolve the honest default-build capability surface from the HAL probe, the
/// compiled features, and the resolved compositor `adapter` (as read by
/// [`crate::capability_warn::read_compositor_adapter`]; `None` when no adapter
/// resolves — the CPU composite path).
///
/// Graceful by construction: an absent/failing backend probes to
/// `available = false` and never panics at startup.
#[must_use]
pub fn resolve_system_capabilities(adapter: Option<&AdapterReport>) -> SystemCapabilities {
    SystemCapabilities {
        backends: probe_backends(),
        compositor: compositor_capability(adapter),
        build: BuildInfo::resolve(
            cfg!(feature = "gpl-codecs"),
            cfg!(feature = "ndi"),
            compiled_features(),
        ),
        ndi_attribution: cfg!(feature = "ndi").then(NdiAttribution::required),
    }
}

/// The codec backends probed at the decode/encode stages (`software` is the
/// universal fallback; the hardware kinds are `Absent` unless the relevant
/// feature is compiled and a device is present).
const CODEC_KINDS: &[HalKind] = &[
    HalKind::Software,
    HalKind::Cuda,
    HalKind::Vaapi,
    HalKind::Qsv,
    HalKind::VideoToolbox,
];

/// Probe the codec backends at the decode + encode stages, plus the software
/// composite path, into DTO rows.
///
/// Only the **software** composite backend is probed: `hal::probe` has no
/// environment probe for the portable *hardware* compositor backends (it returns
/// `BackendUnavailable`), so their real acceleration tier is reported
/// authoritatively by [`SystemCapabilities::compositor`] (the resolved adapter)
/// rather than as a misleading `available: false` row. Graceful: a `probe` error
/// (feature off / no device) becomes `available: false`, never a panic.
fn probe_backends() -> Vec<BackendCapability> {
    let mut backends = Vec::new();
    for &kind in CODEC_KINDS {
        backends.push(one_backend(kind, Stage::Decode));
    }
    backends.push(one_backend(HalKind::Software, Stage::Composite));
    for &kind in CODEC_KINDS {
        backends.push(one_backend(kind, Stage::Encode));
    }
    backends
}

/// Probe one `(kind, stage)` and map the result onto a DTO row.
fn one_backend(kind: HalKind, stage: Stage) -> BackendCapability {
    match multiview_hal::probe::probe(kind, stage) {
        Ok(cap) => BackendCapability {
            kind: dto_backend_kind(kind),
            stage: dto_stage(stage),
            available: true,
            max_resolution: Some(BackendResolution {
                width: cap.max_resolution.width,
                height: cap.max_resolution.height,
            }),
            // `decode_resize` is only meaningful on the decode stage.
            decode_resize: match stage {
                Stage::Decode => Some(cap.decode_resize),
                _ => None,
            },
        },
        Err(_) => BackendCapability {
            kind: dto_backend_kind(kind),
            stage: dto_stage(stage),
            available: false,
            max_resolution: None,
            decode_resize: None,
        },
    }
}

/// Classify the resolved compositor `adapter` into the DTO tier + device/driver.
fn compositor_capability(adapter: Option<&AdapterReport>) -> CompositorCapability {
    let class = dto_compositor_class(AdapterClass::classify(adapter));
    match adapter {
        Some(report) => CompositorCapability {
            class,
            device_type: Some(dto_device_type(report.device_type)),
            driver: Some(report.driver.clone()),
        },
        None => CompositorCapability {
            class,
            device_type: None,
            driver: None,
        },
    }
}

/// Map the core backend kind onto the wire enum. `Software` and any future
/// `#[non_exhaustive]` kind fold into the wildcard (the software row).
fn dto_backend_kind(kind: HalKind) -> BackendKind {
    match kind {
        HalKind::Cuda => BackendKind::Cuda,
        HalKind::VideoToolbox => BackendKind::VideoToolbox,
        HalKind::Vaapi => BackendKind::Vaapi,
        HalKind::Qsv => BackendKind::Qsv,
        HalKind::Wgpu => BackendKind::Wgpu,
        HalKind::Metal => BackendKind::Metal,
        // `HalKind::Software` (and any future `#[non_exhaustive]` kind) falls back
        // to the software row.
        _ => BackendKind::Software,
    }
}

/// Map the HAL stage onto the wire enum. `Composite` and any future stage fold
/// into the wildcard.
fn dto_stage(stage: Stage) -> PipelineStage {
    match stage {
        Stage::Decode => PipelineStage::Decode,
        Stage::Encode => PipelineStage::Encode,
        // `Stage::Composite` (and any future stage) maps to composite.
        _ => PipelineStage::Composite,
    }
}

/// Map the SA-0 adapter class onto the wire enum. `None` and any future class
/// fold into the wildcard.
fn dto_compositor_class(class: AdapterClass) -> CompositorClass {
    match class {
        AdapterClass::Gpu => CompositorClass::Gpu,
        AdapterClass::LowConfidenceGpu => CompositorClass::LowConfidenceGpu,
        AdapterClass::Software => CompositorClass::Software,
        // `AdapterClass::None` (and any future class) maps to `none`.
        _ => CompositorClass::None,
    }
}

/// Map the adapter device type onto the wire enum. `Other` and any future type
/// fold into the wildcard.
fn dto_device_type(device_type: multiview_hal::AdapterDeviceType) -> CompositorDeviceType {
    use multiview_hal::AdapterDeviceType;
    match device_type {
        AdapterDeviceType::DiscreteGpu => CompositorDeviceType::DiscreteGpu,
        AdapterDeviceType::IntegratedGpu => CompositorDeviceType::IntegratedGpu,
        AdapterDeviceType::VirtualGpu => CompositorDeviceType::VirtualGpu,
        AdapterDeviceType::Cpu => CompositorDeviceType::Cpu,
        // `AdapterDeviceType::Other` (and any future type) maps to `other`.
        _ => CompositorDeviceType::Other,
    }
}

/// The compiled, capability/licence-relevant Cargo features of this binary.
///
/// Only real declared `multiview-cli` features appear here — `cfg!` on an
/// undeclared feature would trip `unexpected_cfgs`. Umbrella presets
/// (`nvidia`/`apple`/`linux-vaapi`/`full`) are omitted: they only turn the leaf
/// features below on.
fn compiled_features() -> Vec<String> {
    let mut features = Vec::new();
    macro_rules! push_if {
        ($feat:literal) => {
            if cfg!(feature = $feat) {
                features.push($feat.to_owned());
            }
        };
    }
    push_if!("software");
    push_if!("ffmpeg");
    push_if!("gpl-codecs");
    push_if!("ndi");
    push_if!("ndi-bindings");
    push_if!("webrtc");
    push_if!("webrtc-native");
    push_if!("cuda");
    push_if!("gpu");
    push_if!("overlay");
    push_if!("display-kms");
    push_if!("rist");
    push_if!("youtube");
    push_if!("ntp");
    push_if!("ptp");
    push_if!("mesh-mdns");
    push_if!("heartbeat");
    push_if!("discovery");
    push_if!("devices-net");
    push_if!("tls");
    push_if!("web");
    features
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_control::system::{
        BackendKind, CompositorDeviceType, EffectiveLicense, PipelineStage,
    };
    use multiview_hal::AdapterDeviceType;

    fn has_available(caps: &SystemCapabilities, kind: BackendKind, stage: PipelineStage) -> bool {
        caps.backends
            .iter()
            .any(|b| b.kind == kind && b.stage == stage && b.available)
    }

    #[test]
    fn software_backends_are_available_on_the_default_build() {
        let caps = resolve_system_capabilities(None);
        assert!(has_available(
            &caps,
            BackendKind::Software,
            PipelineStage::Decode
        ));
        assert!(has_available(
            &caps,
            BackendKind::Software,
            PipelineStage::Composite
        ));
        assert!(has_available(
            &caps,
            BackendKind::Software,
            PipelineStage::Encode
        ));
    }

    #[test]
    fn resolved_capabilities_report_the_compiled_build_profile() {
        let caps = resolve_system_capabilities(None);

        // `effective_license` is the codec-linking BUILD-PROFILE licence (AGENTS.md §G /
        // ADR-0012): linking x264/x265 via `gpl-codecs` makes the whole build GPL; the
        // default FFmpeg-LGPL link is LGPL-clean. Gate the expectation on the profile THIS
        // test binary is compiled with, so the compliance-sensitive combos are actually
        // asserted here rather than silently skipped because CI only builds default
        // features. (The exhaustive per-combo mapping is unit-tested in `multiview-control`
        // via `BuildInfo::resolve(bool, bool, …)`; this asserts the compile-time cfg bridge
        // selects the matching profile.)
        #[cfg(feature = "gpl-codecs")]
        assert_eq!(caps.build.effective_license, EffectiveLicense::Gpl);
        #[cfg(not(feature = "gpl-codecs"))]
        assert_eq!(caps.build.effective_license, EffectiveLicense::LgplClean);

        // No `nonfree` Cargo feature exists, so every shippable profile is redistributable.
        assert!(caps.build.redistributable);

        // NDI attribution is mandatory iff the proprietary NDI SDK seam is linked (`ndi`),
        // and is orthogonal to the codec licence.
        #[cfg(feature = "ndi")]
        assert!(caps.ndi_attribution.is_some());
        #[cfg(not(feature = "ndi"))]
        assert!(caps.ndi_attribution.is_none());
    }

    #[test]
    fn a_resolved_adapter_flows_through_to_the_compositor() {
        let report = AdapterReport {
            device_type: AdapterDeviceType::DiscreteGpu,
            driver: "test-driver".to_owned(),
            is_gl_backend: false,
        };
        let caps = resolve_system_capabilities(Some(&report));
        // A resolved adapter is classified (not `None`) and its device/driver
        // flow through unchanged.
        assert_ne!(caps.compositor.class, CompositorClass::None);
        assert_eq!(
            caps.compositor.device_type,
            Some(CompositorDeviceType::DiscreteGpu)
        );
        assert_eq!(caps.compositor.driver.as_deref(), Some("test-driver"));
    }
}
