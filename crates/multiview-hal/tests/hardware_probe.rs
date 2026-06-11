#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Feature-gated hardware probing behaviour.
//!
//! These assertions hold for the pure-Rust default build *and* for any
//! single hardware feature enabled in isolation: on a machine with no such
//! device present (CI), the probe must report [`Error::BackendUnavailable`]
//! cleanly — never panic, never link or call a native library it cannot find.
//!
//! The injectable [`DeviceProbe`] seam is exercised here to prove both arms of
//! the detection contract (device present -> capability; device absent ->
//! unavailable) without requiring real hardware in CI.

use multiview_core::pixel::PixelFormat;
use multiview_hal::probe::{
    detect, DeviceCaps, DeviceProbe, HardwareKind, ProbeOutcome, StageSupport,
};
use multiview_hal::{BackendKind, Error, Stage};

/// A test double that reports a fixed outcome for every query.
struct FixedProbe(ProbeOutcome);

impl DeviceProbe for FixedProbe {
    fn detect(&self, _kind: HardwareKind, _stage: Stage) -> ProbeOutcome {
        self.0.clone()
    }
}

#[test]
fn absent_device_detects_unavailable_for_every_hardware_kind() {
    let probe = FixedProbe(ProbeOutcome::Absent {
        reason: "no device node",
    });
    for kind in HardwareKind::ALL {
        for stage in Stage::ALL {
            let err = detect(&probe, kind, stage).unwrap_err();
            match err {
                Error::BackendUnavailable {
                    kind: reported,
                    reason,
                } => {
                    assert_eq!(reported, kind.backend_kind());
                    assert!(!reason.is_empty());
                }
                other => panic!("expected BackendUnavailable, got {other:?}"),
            }
        }
    }
}

#[test]
fn present_device_yields_a_valid_capability_with_max_resolution_and_formats() {
    let caps = DeviceCaps {
        max_resolution: multiview_hal::Resolution::UHD4K,
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
    };
    let probe = FixedProbe(ProbeOutcome::Present(caps));

    let decode = detect(&probe, HardwareKind::Cuda, Stage::Decode).expect("decode supported");
    assert_eq!(decode.kind, BackendKind::Cuda);
    assert_eq!(decode.stage, Stage::Decode);
    assert_eq!(decode.max_resolution, multiview_hal::Resolution::UHD4K);
    assert!(decode.supports_format(PixelFormat::Nv12));
    assert!(decode.supports_format(PixelFormat::P010));
    // NVDEC fused-resize lever flows through only on the decode stage.
    assert!(decode.decode_resize);
    decode.validate().expect("structurally valid");

    let encode = detect(&probe, HardwareKind::Cuda, Stage::Encode).expect("encode supported");
    assert_eq!(encode.stage, Stage::Encode);
    // decode_resize is meaningless off the decode stage and must be cleared so
    // the descriptor validates.
    assert!(!encode.decode_resize);
    encode.validate().expect("structurally valid");
}

#[test]
fn present_device_without_a_stage_reports_unavailable_for_that_stage() {
    let caps = DeviceCaps {
        max_resolution: multiview_hal::Resolution::HD1080,
        formats: vec![PixelFormat::Nv12],
        decode: StageSupport::Supported {
            decode_resize: false,
        },
        // This device does not encode (e.g. a decode-only ASIC).
        encode: StageSupport::Unsupported,
        scale: StageSupport::Supported {
            decode_resize: false,
        },
    };
    let probe = FixedProbe(ProbeOutcome::Present(caps));

    assert!(detect(&probe, HardwareKind::Vaapi, Stage::Decode).is_ok());
    let err = detect(&probe, HardwareKind::Vaapi, Stage::Encode).unwrap_err();
    assert!(matches!(err, Error::BackendUnavailable { .. }));
}

#[test]
fn hardware_kind_maps_to_the_expected_backend_kind() {
    assert_eq!(HardwareKind::Cuda.backend_kind(), BackendKind::Cuda);
    assert_eq!(HardwareKind::Vaapi.backend_kind(), BackendKind::Vaapi);
    assert_eq!(HardwareKind::Qsv.backend_kind(), BackendKind::Qsv);
    assert_eq!(
        HardwareKind::VideoToolbox.backend_kind(),
        BackendKind::VideoToolbox
    );
}

/// Assert a real `probe(kind, ...)` resolves cleanly: on a host with the device
/// it returns a validated capability; on a host without it (CI) it returns
/// `BackendUnavailable`. Never panics either way.
///
/// Only compiled when at least one hardware feature is on (the default,
/// feature-off build has no caller and would flag this as dead code).
#[cfg(any(
    feature = "cuda",
    feature = "vaapi",
    feature = "qsv",
    feature = "videotoolbox"
))]
fn probe_resolves_cleanly(kind: BackendKind) {
    for stage in Stage::ALL {
        match multiview_hal::probe::probe(kind, stage) {
            Ok(cap) => {
                assert_eq!(cap.kind, kind);
                assert_eq!(cap.stage, stage);
                cap.validate()
                    .expect("a present device must report a valid capability");
            }
            Err(Error::BackendUnavailable {
                kind: reported,
                reason,
            }) => {
                assert_eq!(reported, kind);
                assert!(!reason.is_empty());
            }
            Err(other) => panic!("probe must only ever fail as BackendUnavailable, got {other:?}"),
        }
    }
}

// One feature-gated test per backend: with the feature compiled in but no such
// device present (CI), `probe` must report `BackendUnavailable` cleanly — it
// must never panic and never link/call a vendor SDK it cannot find. On a host
// that *does* have the device, the `Ok` arm validates the reported capability.

#[cfg(feature = "cuda")]
#[test]
fn cuda_probe_is_graceful_when_feature_on() {
    probe_resolves_cleanly(BackendKind::Cuda);
}

#[cfg(feature = "vaapi")]
#[test]
fn vaapi_probe_is_graceful_when_feature_on() {
    probe_resolves_cleanly(BackendKind::Vaapi);
}

#[cfg(feature = "qsv")]
#[test]
fn qsv_probe_is_graceful_when_feature_on() {
    probe_resolves_cleanly(BackendKind::Qsv);
}

#[cfg(feature = "videotoolbox")]
#[test]
fn videotoolbox_probe_is_graceful_when_feature_on() {
    probe_resolves_cleanly(BackendKind::VideoToolbox);
}
