//! System capability reporting for `GET /api/v1/system/capabilities` (ADR-W030).
//!
//! The honest **default-build** capability surface: which codec/compositor
//! backends this binary can use, the SA-0 compositor classification, the
//! effective build-profile **licence** (a compliance surface — ADR-0012), and
//! the mandatory NDI attribution. This module holds the wire DTO only; it is
//! deliberately built from **primitives and local enums — no `multiview-hal`
//! types** — so `multiview-control` keeps zero dependency on the HAL. The
//! `multiview` binary maps `hal::probe()` + compiled Cargo features + the
//! resolved compositor `AdapterReport` onto this DTO at startup and installs it
//! with [`crate::AppState::with_capabilities`] (a static snapshot — invariant
//! #10, never an engine channel).
//!
//! The rich per-device telemetry sketched in the capability matrix §3.4
//! (per-codec profiles, NVENC session budget, VRAM, host PSI) is **not** modelled
//! here: it needs feature-gated backend code and GPU-hardware validation and is
//! the separate tracked lane (task #180). Only fields the default/software build
//! honestly fills appear below (rule 6 / rule 27).

use serde::Serialize;

/// The pipeline stage a backend serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PipelineStage {
    /// Decode (demux/decode of an input elementary stream).
    Decode,
    /// Composite (scale + place + colour-convert + blend the canvas).
    Composite,
    /// Encode (encode the composited canvas for a rendition).
    Encode,
}

/// The concrete kind of a codec/compositor backend. Serialized to match the
/// Cargo feature names (`cuda`, `vaapi`, `qsv`, `videotoolbox`, `wgpu`, `metal`,
/// `software`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum BackendKind {
    /// Pure-CPU / SIMD software path (always available; the universal fallback).
    Software,
    /// NVIDIA CUDA (NVDEC/NVENC + custom CUDA compositor).
    Cuda,
    /// Apple `VideoToolbox`.
    #[serde(rename = "videotoolbox")]
    VideoToolbox,
    /// Linux VA-API (Intel/AMD).
    Vaapi,
    /// Intel Quick Sync via oneVPL.
    Qsv,
    /// Portable wgpu compositor backend.
    Wgpu,
    /// Apple Metal compositor backend.
    Metal,
}

/// A backend's maximum supported resolution (as probed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BackendResolution {
    /// Maximum frame width in pixels.
    pub width: u32,
    /// Maximum frame height in pixels.
    pub height: u32,
}

/// The availability + probed capability of one `(kind, stage)` backend.
///
/// `available` is the honest single signal: the backend is compiled **and** a
/// usable device is present. The compiled-but-device-absent distinction
/// (`compiled_in`) needs HAL-side feature introspection and is deferred to the
/// SA-1+ deep probe (task #180).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BackendCapability {
    /// The backend kind.
    pub kind: BackendKind,
    /// The pipeline stage this entry describes.
    pub stage: PipelineStage,
    /// Whether the backend is compiled in **and** a usable device is present.
    pub available: bool,
    /// The probed maximum resolution — present only when `available`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_resolution: Option<BackendResolution>,
    /// Whether the backend can resize during decode — present only on an
    /// available decode-stage backend (meaningless elsewhere).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_resize: Option<bool>,
}

/// The SA-0 composite-usability classification of the resolved wgpu adapter
/// (ADR-0035): the acceleration tier the compositor actually runs on. `None`
/// means no adapter resolved — the CPU composite path only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CompositorClass {
    /// A real hardware GPU adapter is present.
    Gpu,
    /// An adapter is present but its GPU-ness is low-confidence.
    LowConfidenceGpu,
    /// A software rasterizer adapter (e.g. llvmpipe).
    Software,
    /// No adapter resolved — CPU composite path only.
    None,
}

/// The kind of graphics device backing the compositor adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CompositorDeviceType {
    /// A discrete GPU.
    DiscreteGpu,
    /// An integrated GPU.
    IntegratedGpu,
    /// A virtual/paravirtual GPU.
    VirtualGpu,
    /// A CPU (software rasterizer).
    Cpu,
    /// Some other/unknown device type.
    Other,
}

/// The compositor's resolved acceleration tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompositorCapability {
    /// The composite-usability class.
    pub class: CompositorClass,
    /// The device type — present only when an adapter resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_type: Option<CompositorDeviceType>,
    /// The adapter's driver string — present only when an adapter resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
}

/// The effective **build-profile** licence — the codec-linking licence of the
/// compiled artifact (ADR-0012 / AGENTS.md §G). Distinct from the project's
/// source-available licence and from NDI's redistribution restriction (carried
/// separately by [`BuildInfo::ndi`] / [`NdiAttribution`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub enum EffectiveLicense {
    /// The default build: `FFmpeg` linked LGPL, no x264/x265, redistributable.
    #[serde(rename = "LGPL-clean")]
    LgplClean,
    /// `gpl-codecs` is compiled (x264/x265) → the whole product is GPL.
    #[serde(rename = "GPL")]
    Gpl,
}

/// The build-profile compliance surface: effective licence, redistributability,
/// the compiled feature set, and whether NDI is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BuildInfo {
    /// The effective codec-linking licence of this artifact.
    pub effective_license: EffectiveLicense,
    /// Whether the artifact is redistributable. `true` for every shippable
    /// Multiview build: the default is LGPL-clean, `gpl-codecs` is GPL
    /// (redistributable under copyleft), and NDI is runtime-loaded (never
    /// vendored/linked) — there is no non-redistributable `--enable-nonfree`
    /// closure (no `nonfree` Cargo feature exists).
    pub redistributable: bool,
    /// The compiled, capability/licence-relevant Cargo features.
    pub features: Vec<String>,
    /// Whether the `ndi` feature is compiled (the proprietary, runtime-loaded
    /// SDK path). When `true`, [`SystemCapabilities::ndi_attribution`] is set.
    pub ndi: bool,
}

impl BuildInfo {
    /// Resolve the compliance surface from the compiled licence-escalating
    /// features. The caller (the `multiview` binary) passes its own
    /// `cfg!(feature = "gpl-codecs")` / `cfg!(feature = "ndi")` and the compiled
    /// feature list.
    ///
    /// Pinned to AGENTS.md §G / ADR-0012: `gpl-codecs` ⇒ the whole product is
    /// GPL; otherwise LGPL-clean. Every shippable build is redistributable.
    #[must_use]
    pub fn resolve(gpl_codecs: bool, ndi: bool, features: Vec<String>) -> Self {
        // `gpl-codecs` (x264/x265) relicenses the whole product GPL; otherwise
        // the default LGPL-clean profile stands (AGENTS.md §G / ADR-0012). Every
        // shippable build is redistributable — there is no `--enable-nonfree`
        // Cargo feature, and NDI is runtime-loaded (never linked/vendored).
        let effective_license = if gpl_codecs {
            EffectiveLicense::Gpl
        } else {
            EffectiveLicense::LgplClean
        };
        Self {
            effective_license,
            redistributable: true,
            features,
            ndi,
        }
    }
}

/// The mandatory NDI attribution (AGENTS.md §G) — present iff the `ndi` feature
/// is compiled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NdiAttribution {
    /// The required trademark line.
    pub trademark: String,
    /// A link to the NDI site.
    pub url: String,
}

impl NdiAttribution {
    /// The mandatory NDI® attribution carried whenever the `ndi` feature is on.
    #[must_use]
    pub fn required() -> Self {
        Self {
            trademark: "NDI® is a registered trademark of Vizrt NDI AB".to_owned(),
            url: "https://ndi.video".to_owned(),
        }
    }
}

/// The `GET /api/v1/system/capabilities` response (ADR-W030): the honest
/// default-build capability + licence surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SystemCapabilities {
    /// Per-`(kind, stage)` backend availability.
    pub backends: Vec<BackendCapability>,
    /// The compositor's resolved acceleration tier.
    pub compositor: CompositorCapability,
    /// The build-profile compliance surface.
    pub build: BuildInfo,
    /// The mandatory NDI attribution — present iff `ndi` is compiled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ndi_attribution: Option<NdiAttribution>,
}

impl Default for SystemCapabilities {
    /// A coarse, honest software-only default so the endpoint is always present
    /// and truthful even when the binary wires nothing (tests, non-run paths).
    /// The software path is always available (the universal fallback); detailed
    /// probed caps are filled by the binary's HAL-backed mapping. No adapter is
    /// assumed (CPU composite), and the default profile is LGPL-clean.
    fn default() -> Self {
        let software = |stage| BackendCapability {
            kind: BackendKind::Software,
            stage,
            available: true,
            max_resolution: None,
            decode_resize: None,
        };
        Self {
            backends: vec![
                software(PipelineStage::Decode),
                software(PipelineStage::Composite),
                software(PipelineStage::Encode),
            ],
            compositor: CompositorCapability {
                class: CompositorClass::None,
                device_type: None,
                driver: None,
            },
            build: BuildInfo::resolve(false, false, vec!["software".to_owned()]),
            ndi_attribution: None,
        }
    }
}
