//! Capability detection / hardware probing.
//!
//! The pure-Rust default build knows exactly one backend exists: the
//! always-available **software** tier (the universal fallback; efficiency §2,
//! core-engine §6.5). Every hardware backend lives behind an off-by-default
//! Cargo feature (`cuda`, `vaapi`, `qsv`, `videotoolbox`); with the feature
//! disabled, [`probe`] returns [`Error::BackendUnavailable`] without touching
//! any native library or GPU — the GPU-free CI contract.
//!
//! When a feature *is* compiled in, probing follows the three-layer model from
//! [core-engine §6.2](../../../docs/research/core-engine.md): the *environment*
//! layer implemented here decides whether a device is even **present** on the
//! host (DRM render nodes for VAAPI/QSV, NVIDIA device nodes / `CUDA_VISIBLE_DEVICES`
//! for CUDA, the OS for `VideoToolbox`) without linking or calling a vendor SDK.
//! The deeper vendor caps queries (NVENC `NV_ENC_CAPS`, `cuvidGetDecoderCaps`,
//! oneVPL `MFXQueryImplsDescription`, `vaQueryConfigProfiles`,
//! `VTCopyVideoEncoderList`) land in the feature-gated backend crates per
//! ADR-0003/0004 and refine the [`DeviceCaps`] this layer produces.
//!
//! The vendor seam is the [`DeviceProbe`] trait: a pure, injectable predicate
//! over `(HardwareKind, Stage)` returning a [`ProbeOutcome`]. The real
//! environment detector ([`EnvProbe`]) implements it; tests inject a double.
//! Crucially, on a machine with **no** device (CI), every real detector returns
//! [`ProbeOutcome::Absent`] and [`detect`] maps that to a clean
//! [`Error::BackendUnavailable`] — never a panic, never a native call.
use multiview_core::pixel::PixelFormat;
use multiview_core::traits::BackendKind;

use crate::capability::{Capability, Resolution, Stage};
use crate::error::{Error, Result};

/// The hardware backend kinds this layer can probe for.
///
/// A strict subset of [`BackendKind`]: only the vendor *media* backends have an
/// environment probe. (The portable `Wgpu`/`Metal` compositor backends and the
/// always-available `Software` tier are not probed here.) Mapping back to a
/// [`BackendKind`] is total via [`HardwareKind::backend_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HardwareKind {
    /// NVIDIA CUDA (NVDEC/NVENC).
    Cuda,
    /// Linux VA-API (Intel/AMD).
    Vaapi,
    /// Intel Quick Sync via oneVPL.
    Qsv,
    /// Apple `VideoToolbox`.
    VideoToolbox,
}

impl HardwareKind {
    /// Every probeable hardware kind, in a stable order.
    pub const ALL: [HardwareKind; 4] = [
        HardwareKind::Cuda,
        HardwareKind::Vaapi,
        HardwareKind::Qsv,
        HardwareKind::VideoToolbox,
    ];

    /// The [`BackendKind`] this hardware kind corresponds to.
    #[must_use]
    pub const fn backend_kind(self) -> BackendKind {
        match self {
            HardwareKind::Cuda => BackendKind::Cuda,
            HardwareKind::Vaapi => BackendKind::Vaapi,
            HardwareKind::Qsv => BackendKind::Qsv,
            HardwareKind::VideoToolbox => BackendKind::VideoToolbox,
        }
    }

    /// The Cargo feature name that gates real probing for this kind.
    #[must_use]
    pub const fn feature_name(self) -> &'static str {
        match self {
            HardwareKind::Cuda => "cuda",
            HardwareKind::Vaapi => "vaapi",
            HardwareKind::Qsv => "qsv",
            HardwareKind::VideoToolbox => "videotoolbox",
        }
    }
}

/// Whether a device supports a given pipeline stage, and (for decode) whether
/// it can resize *during* decode.
///
/// This is the per-stage half of [`DeviceCaps`]; the planner only ever sees the
/// resulting [`Capability`], never this intermediate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StageSupport {
    /// The device implements this stage.
    Supported {
        /// Decode-only: fused decode-time resize (the NVDEC `-resize` lever).
        /// Ignored (and forced to `false` in the [`Capability`]) off the decode
        /// stage.
        decode_resize: bool,
    },
    /// The device does not implement this stage (e.g. a decode-only ASIC, or a
    /// platform with no hardware encoder for the requested codec).
    Unsupported,
}

impl StageSupport {
    /// Whether this stage is supported.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, StageSupport::Supported { .. })
    }
}

/// The capabilities a present device reports.
///
/// Produced by a [`DeviceProbe`] when a device is found. `max_resolution` and
/// `formats` describe the device as a whole; the three [`StageSupport`] fields
/// say which of decode/encode/scale it can perform. [`detect`] turns the
/// `(DeviceCaps, Stage)` pair into a single-stage [`Capability`] for the
/// registry/planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCaps {
    /// Maximum resolution the device handles (inclusive ceiling on both axes).
    pub max_resolution: Resolution,
    /// Pixel formats the device accepts (must be non-empty for a usable
    /// device).
    pub formats: Vec<PixelFormat>,
    /// Decode-stage support.
    pub decode: StageSupport,
    /// Encode-stage support.
    pub encode: StageSupport,
    /// Scale/composite-stage support (the VPP/SFC or compositor path).
    pub scale: StageSupport,
}

impl DeviceCaps {
    /// The [`StageSupport`] for a pipeline [`Stage`].
    #[must_use]
    pub const fn support_for(&self, stage: Stage) -> StageSupport {
        match stage {
            Stage::Decode => self.decode,
            Stage::Composite => self.scale,
            Stage::Encode => self.encode,
        }
    }
}

/// The outcome of probing one `(HardwareKind, Stage)` query.
///
/// `Absent` carries a static reason that flows into the
/// [`Error::BackendUnavailable`] surfaced to the planner.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeOutcome {
    /// A device is present; here are its capabilities.
    Present(DeviceCaps),
    /// No usable device for this kind on this host.
    Absent {
        /// Static, human-readable reason (e.g. `"no DRM render node"`).
        reason: &'static str,
    },
}

/// The vendor-probe seam: decide, for a `(HardwareKind, Stage)`, whether a
/// device is present and what it can do.
///
/// Implemented for real by [`EnvProbe`] (environment detection, no native SDK)
/// and stubbed by test doubles. Keeping this a trait makes detection
/// unit-testable for both the present and absent arms without real hardware.
pub trait DeviceProbe {
    /// Probe for `kind` at `stage`.
    fn detect(&self, kind: HardwareKind, stage: Stage) -> ProbeOutcome;
}

/// Resolve a `(HardwareKind, Stage)` query through a [`DeviceProbe`] into a
/// single-stage [`Capability`].
///
/// On [`ProbeOutcome::Present`] with the stage supported, builds a validated
/// [`Capability`] carrying the device's max resolution and formats (and, on the
/// decode stage only, its fused-resize flag). On [`ProbeOutcome::Absent`], or a
/// present device that does not implement the requested stage, returns a clean
/// [`Error::BackendUnavailable`].
///
/// # Errors
///
/// Returns [`Error::BackendUnavailable`] when no device is present or the stage
/// is unsupported, and [`Error::InvalidCapability`] if a present device reports
/// a structurally impossible descriptor (zero resolution / empty format list).
pub fn detect<P: DeviceProbe + ?Sized>(
    probe: &P,
    kind: HardwareKind,
    stage: Stage,
) -> Result<Capability> {
    let backend = kind.backend_kind();
    match probe.detect(kind, stage) {
        ProbeOutcome::Absent { reason } => Err(Error::BackendUnavailable {
            kind: backend,
            reason,
        }),
        ProbeOutcome::Present(caps) => match caps.support_for(stage) {
            StageSupport::Unsupported => Err(Error::BackendUnavailable {
                kind: backend,
                reason: "device present but does not implement this stage",
            }),
            StageSupport::Supported { decode_resize } => {
                // `decode_resize` is only meaningful on the decode stage; clear
                // it elsewhere so the descriptor validates (capability.rs).
                let resize = decode_resize && stage == Stage::Decode;
                let capability = Capability::new(backend, stage, caps.max_resolution, caps.formats)
                    .with_decode_resize(resize);
                capability.validate()?;
                Ok(capability)
            }
        },
    }
}

/// The real environment-detection probe.
///
/// Implements [`DeviceProbe`] by inspecting the host *without* linking or
/// calling any vendor SDK: it answers only the L1/presence question from
/// [core-engine §6.2](../../../docs/research/core-engine.md). For a kind whose
/// feature is not compiled in, it always reports [`ProbeOutcome::Absent`]. With
/// the feature on, it checks for the relevant device node / OS and, if present,
/// reports a conservative baseline [`DeviceCaps`] that the feature-gated backend
/// crate later refines with true vendor caps queries.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct EnvProbe;

impl EnvProbe {
    /// Construct the environment probe.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DeviceProbe for EnvProbe {
    fn detect(&self, kind: HardwareKind, _stage: Stage) -> ProbeOutcome {
        match kind {
            HardwareKind::Cuda => detect_cuda(),
            HardwareKind::Vaapi => detect_vaapi(),
            HardwareKind::Qsv => detect_qsv(),
            HardwareKind::VideoToolbox => detect_videotoolbox(),
        }
    }
}

// ----------------------------------------------------------------------------
// Per-kind environment detectors.
//
// Each has a feature-off arm (always Absent — the GPU-free CI contract) and a
// feature-on arm that performs presence detection with std only. The feature-on
// arms must still report Absent *cleanly* on a host with no device (CI), which
// is exactly what the feature-gated probe test asserts.
// ----------------------------------------------------------------------------

#[cfg(not(feature = "cuda"))]
const fn detect_cuda() -> ProbeOutcome {
    ProbeOutcome::Absent {
        reason: "cuda feature not enabled",
    }
}

#[cfg(feature = "cuda")]
fn detect_cuda() -> ProbeOutcome {
    // Presence layer only: an NVIDIA device node under /dev (nvidia0/nvidiactl)
    // or an explicit CUDA_VISIBLE_DEVICES that names a device. Real NVDEC/NVENC
    // caps (NV_ENC_CAPS / cuvidGetDecoderCaps) are queried by the cuda backend
    // crate; absent a device, we never reach that native code.
    if std::env::var_os("CUDA_VISIBLE_DEVICES").is_some_and(|v| !v.is_empty() && v != "-1")
        || std::path::Path::new("/dev/nvidiactl").exists()
        || std::path::Path::new("/dev/nvidia0").exists()
    {
        // Conservative NVIDIA baseline: NVDEC decode-time resize is the
        // signature lever (efficiency §1). Refined later by the backend crate.
        return ProbeOutcome::Present(DeviceCaps {
            max_resolution: Resolution::UHD4K,
            formats: vec![PixelFormat::Nv12, PixelFormat::P010],
            decode: StageSupport::Supported {
                decode_resize: true,
            },
            encode: StageSupport::Supported {
                decode_resize: false,
            },
            scale: StageSupport::Supported {
                decode_resize: false,
            },
        });
    }
    ProbeOutcome::Absent {
        reason: "no NVIDIA device node and CUDA_VISIBLE_DEVICES unset",
    }
}

#[cfg(not(feature = "vaapi"))]
const fn detect_vaapi() -> ProbeOutcome {
    ProbeOutcome::Absent {
        reason: "vaapi feature not enabled",
    }
}

#[cfg(feature = "vaapi")]
fn detect_vaapi() -> ProbeOutcome {
    // Presence layer: a DRM render node (LIBVA_DRIVER honoured by the backend
    // crate). Real profiles/entrypoints come from vaQueryConfigProfiles later.
    if has_drm_render_node() {
        // Intel/AMD budget a full-res decode surface (no fused decode resize);
        // VPP/SFC handles scale post-decode (efficiency §1).
        return ProbeOutcome::Present(DeviceCaps {
            max_resolution: Resolution::UHD4K,
            formats: vec![PixelFormat::Nv12, PixelFormat::P010],
            decode: StageSupport::Supported {
                decode_resize: false,
            },
            encode: StageSupport::Supported {
                decode_resize: false,
            },
            scale: StageSupport::Supported {
                decode_resize: false,
            },
        });
    }
    ProbeOutcome::Absent {
        reason: "no DRM render node under /dev/dri",
    }
}

#[cfg(not(feature = "qsv"))]
const fn detect_qsv() -> ProbeOutcome {
    ProbeOutcome::Absent {
        reason: "qsv feature not enabled",
    }
}

#[cfg(feature = "qsv")]
fn detect_qsv() -> ProbeOutcome {
    // QSV is derived from VAAPI on Linux: same DRM render-node presence gate.
    // oneVPL MFXQueryImplsDescription refines this in the backend crate.
    if has_drm_render_node() {
        return ProbeOutcome::Present(DeviceCaps {
            max_resolution: Resolution::UHD4K,
            formats: vec![PixelFormat::Nv12, PixelFormat::P010],
            decode: StageSupport::Supported {
                decode_resize: false,
            },
            encode: StageSupport::Supported {
                decode_resize: false,
            },
            scale: StageSupport::Supported {
                decode_resize: false,
            },
        });
    }
    ProbeOutcome::Absent {
        reason: "no DRM render node under /dev/dri (QSV derives from VAAPI)",
    }
}

#[cfg(not(feature = "videotoolbox"))]
const fn detect_videotoolbox() -> ProbeOutcome {
    ProbeOutcome::Absent {
        reason: "videotoolbox feature not enabled",
    }
}

#[cfg(feature = "videotoolbox")]
fn detect_videotoolbox() -> ProbeOutcome {
    // VideoToolbox exists on every macOS host; off macOS it is absent. The
    // backend crate refines with VTCopyVideoEncoderList / VTIsHardwareDecodeSupported.
    if cfg!(target_os = "macos") {
        return ProbeOutcome::Present(DeviceCaps {
            max_resolution: Resolution::UHD4K,
            formats: vec![PixelFormat::Nv12, PixelFormat::P010],
            // VT can produce a scaled decode surface; encode is the cleanest
            // path on Apple (VT->IOSurface->Metal->VT; core-engine §6.4).
            decode: StageSupport::Supported {
                decode_resize: true,
            },
            encode: StageSupport::Supported {
                decode_resize: false,
            },
            scale: StageSupport::Supported {
                decode_resize: false,
            },
        });
    }
    ProbeOutcome::Absent {
        reason: "VideoToolbox is only available on macOS",
    }
}

/// Whether any DRM render node (`/dev/dri/renderD*`) exists on this host.
///
/// Shared by the VAAPI and QSV detectors. Returns `false` cleanly when
/// `/dev/dri` is absent or unreadable (CI without a GPU).
#[cfg(any(feature = "vaapi", feature = "qsv"))]
fn has_drm_render_node() -> bool {
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("renderD"))
    })
}

/// The always-available software capability for `stage`.
///
/// Software is the universal fallback (invariant #9's floor): it accepts NV12
/// (and 10-bit P010) at any practical resolution. The ceiling here is a large
/// sentinel (8K) so software never *capability*-rejects a real tile — its true
/// limit is throughput, modeled by the [`crate::cost`] budget, not a hard
/// resolution cap.
#[must_use]
pub fn software_capability(stage: Stage) -> Capability {
    // 8K ceiling: software has no fixed-function resolution limit; the cost
    // budget is what bounds it.
    const SOFTWARE_MAX: Resolution = Resolution::new(7680, 4320);
    Capability::new(
        BackendKind::Software,
        stage,
        SOFTWARE_MAX,
        vec![PixelFormat::Nv12, PixelFormat::P010],
    )
}

/// Probe for a backend's capability at `stage`, using the real environment
/// detector ([`EnvProbe`]).
///
/// [`BackendKind::Software`] always succeeds. Every hardware kind is resolved
/// through [`detect`]: with its feature off it is unavailable; with the feature
/// on it is available only when a real device is present (so CI, with no device,
/// still gets a clean [`Error::BackendUnavailable`]).
///
/// # Errors
///
/// Returns [`Error::BackendUnavailable`] for any hardware [`BackendKind`] whose
/// device is absent (feature off, or no device found), and for backend kinds
/// without an environment probe (the portable compositor backends).
pub fn probe(kind: BackendKind, stage: Stage) -> Result<Capability> {
    match kind {
        BackendKind::Software => Ok(software_capability(stage)),
        BackendKind::Cuda => detect(&EnvProbe, HardwareKind::Cuda, stage),
        BackendKind::Vaapi => detect(&EnvProbe, HardwareKind::Vaapi, stage),
        BackendKind::Qsv => detect(&EnvProbe, HardwareKind::Qsv, stage),
        BackendKind::VideoToolbox => detect(&EnvProbe, HardwareKind::VideoToolbox, stage),
        // Compositor-only / portable backend kinds: no environment probe.
        other => Err(Error::BackendUnavailable {
            kind: other,
            reason: "no environment probe for this backend kind",
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// A probe double returning a fixed outcome.
    struct Fixed(ProbeOutcome);
    impl DeviceProbe for Fixed {
        fn detect(&self, _kind: HardwareKind, _stage: Stage) -> ProbeOutcome {
            self.0.clone()
        }
    }

    fn full_caps() -> DeviceCaps {
        DeviceCaps {
            max_resolution: Resolution::UHD4K,
            formats: vec![PixelFormat::Nv12, PixelFormat::P010],
            decode: StageSupport::Supported {
                decode_resize: true,
            },
            encode: StageSupport::Supported {
                decode_resize: false,
            },
            scale: StageSupport::Supported {
                decode_resize: false,
            },
        }
    }

    #[test]
    fn detect_maps_absent_to_backend_unavailable() {
        let probe = Fixed(ProbeOutcome::Absent { reason: "nope" });
        let err = detect(&probe, HardwareKind::Cuda, Stage::Decode).unwrap_err();
        assert_eq!(
            err,
            Error::BackendUnavailable {
                kind: BackendKind::Cuda,
                reason: "nope",
            }
        );
    }

    #[test]
    fn detect_present_builds_validated_capability() {
        let probe = Fixed(ProbeOutcome::Present(full_caps()));
        let cap = detect(&probe, HardwareKind::Cuda, Stage::Decode).unwrap();
        assert_eq!(cap.kind, BackendKind::Cuda);
        assert_eq!(cap.max_resolution, Resolution::UHD4K);
        assert!(cap.decode_resize);
        cap.validate().unwrap();
    }

    #[test]
    fn decode_resize_is_cleared_off_the_decode_stage() {
        let probe = Fixed(ProbeOutcome::Present(full_caps()));
        let cap = detect(&probe, HardwareKind::Cuda, Stage::Encode).unwrap();
        // full_caps reports decode_resize on decode only; encode must clear it.
        assert!(!cap.decode_resize);
        cap.validate().unwrap();
    }

    #[test]
    fn unsupported_stage_reports_unavailable() {
        let mut caps = full_caps();
        caps.encode = StageSupport::Unsupported;
        let probe = Fixed(ProbeOutcome::Present(caps));
        assert!(detect(&probe, HardwareKind::Vaapi, Stage::Decode).is_ok());
        assert!(detect(&probe, HardwareKind::Vaapi, Stage::Encode).is_err());
    }

    #[test]
    fn env_probe_is_clean_when_feature_off_or_no_device() {
        // In the default (feature-off) CI build EnvProbe must report Absent for
        // every kind/stage — no panic, no native call.
        for kind in HardwareKind::ALL {
            for stage in Stage::ALL {
                match EnvProbe.detect(kind, stage) {
                    ProbeOutcome::Absent { reason } => assert!(!reason.is_empty()),
                    // With a feature on and a real device present this could be
                    // Present; that arm is exercised on hardware, not here.
                    ProbeOutcome::Present(caps) => {
                        assert!(!caps.formats.is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn probe_software_always_succeeds() {
        for stage in Stage::ALL {
            let cap = probe(BackendKind::Software, stage).unwrap();
            assert_eq!(cap.kind, BackendKind::Software);
        }
    }

    #[test]
    fn hardware_kind_round_trips_to_backend_kind() {
        for kind in HardwareKind::ALL {
            let backend = kind.backend_kind();
            assert!(matches!(
                backend,
                BackendKind::Cuda
                    | BackendKind::Vaapi
                    | BackendKind::Qsv
                    | BackendKind::VideoToolbox
            ));
            assert!(!kind.feature_name().is_empty());
        }
    }
}
