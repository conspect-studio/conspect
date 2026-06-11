#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Probe behaviour: software is always available; hardware backends report
//! `BackendUnavailable` in the default (feature-off) build.

use multiview_core::pixel::PixelFormat;
use multiview_hal::probe::{probe, software_capability};
use multiview_hal::{BackendKind, Error, Stage};

#[test]
fn software_probe_always_succeeds_for_every_stage() {
    for stage in Stage::ALL {
        let cap = probe(BackendKind::Software, stage).expect("software always available");
        assert_eq!(cap.kind, BackendKind::Software);
        assert_eq!(cap.stage, stage);
        assert!(cap.supports_format(PixelFormat::Nv12));
        cap.validate()
            .expect("software capability is structurally valid");
    }
}

#[test]
fn software_capability_helper_matches_probe() {
    let probed = probe(BackendKind::Software, Stage::Encode).unwrap();
    let direct = software_capability(Stage::Encode);
    assert_eq!(probed, direct);
}

#[test]
fn hardware_backends_are_unavailable_in_default_build() {
    // In the pure-Rust CI build none of these features are enabled, so every
    // hardware probe must report unavailable rather than touch a native lib.
    for kind in [
        BackendKind::Cuda,
        BackendKind::Vaapi,
        BackendKind::Qsv,
        BackendKind::VideoToolbox,
    ] {
        let err = probe(kind, Stage::Decode).unwrap_err();
        match err {
            Error::BackendUnavailable {
                kind: reported,
                reason,
            } => {
                assert_eq!(reported, kind);
                assert!(!reason.is_empty());
            }
            other => panic!("expected BackendUnavailable for {kind:?}, got {other:?}"),
        }
    }
}
