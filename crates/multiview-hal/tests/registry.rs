#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Registry behaviour: register / query by `(stage, kind)`, duplicate
//! rejection, software defaults, and deterministic ordering.

use multiview_core::pixel::PixelFormat;
use multiview_hal::{BackendKind, BackendRegistry, Capability, Error, Resolution, Stage};

fn cuda_decode() -> Capability {
    Capability::new(
        BackendKind::Cuda,
        Stage::Decode,
        Resolution::UHD4K,
        vec![PixelFormat::Nv12, PixelFormat::P010],
    )
}

#[test]
fn register_then_get_returns_the_capability() {
    let mut registry = BackendRegistry::new();
    registry.register(cuda_decode()).unwrap();

    let found = registry
        .get(Stage::Decode, BackendKind::Cuda)
        .expect("registered capability must be found");
    assert_eq!(found.kind, BackendKind::Cuda);
    assert_eq!(found.stage, Stage::Decode);
    assert_eq!(found.max_resolution, Resolution::UHD4K);
    assert!(found.supports_format(PixelFormat::P010));
}

#[test]
fn get_for_unregistered_pair_is_none_but_other_pairs_unaffected() {
    let mut registry = BackendRegistry::new();
    registry.register(cuda_decode()).unwrap();

    // Same kind, different stage: not registered.
    assert!(registry.get(Stage::Encode, BackendKind::Cuda).is_none());
    // Same stage, different kind: not registered.
    assert!(registry.get(Stage::Decode, BackendKind::Vaapi).is_none());
    // The one we did register is still there.
    assert!(registry.contains(Stage::Decode, BackendKind::Cuda));
}

#[test]
fn require_returns_backend_not_found_error_with_the_queried_keys() {
    let registry = BackendRegistry::new();
    let err = registry
        .require(Stage::Encode, BackendKind::Vaapi)
        .unwrap_err();
    assert_eq!(
        err,
        Error::BackendNotFound {
            stage: Stage::Encode,
            kind: BackendKind::Vaapi,
        }
    );
}

#[test]
fn duplicate_registration_is_rejected_and_does_not_overwrite() {
    let mut registry = BackendRegistry::new();
    registry.register(cuda_decode()).unwrap();

    // A second register for the same (stage, kind) must fail...
    let mut second = cuda_decode();
    second.max_resolution = Resolution::HD720;
    let err = registry.register(second).unwrap_err();
    assert_eq!(
        err,
        Error::DuplicateBackend {
            stage: Stage::Decode,
            kind: BackendKind::Cuda,
        }
    );

    // ...and the original capability is untouched (not the HD720 overwrite).
    let found = registry.get(Stage::Decode, BackendKind::Cuda).unwrap();
    assert_eq!(found.max_resolution, Resolution::UHD4K);
}

#[test]
fn replace_overwrites_and_returns_the_previous() {
    let mut registry = BackendRegistry::new();
    registry.register(cuda_decode()).unwrap();

    let mut replacement = cuda_decode();
    replacement.max_resolution = Resolution::HD1080;
    let previous = registry.replace(replacement).unwrap();

    let previous = previous.expect("replace must return the prior capability");
    assert_eq!(previous.max_resolution, Resolution::UHD4K);
    assert_eq!(
        registry
            .get(Stage::Decode, BackendKind::Cuda)
            .unwrap()
            .max_resolution,
        Resolution::HD1080
    );
}

#[test]
fn registering_an_invalid_capability_is_rejected() {
    let mut registry = BackendRegistry::new();

    // Empty format list.
    let bad = Capability::new(BackendKind::Cuda, Stage::Decode, Resolution::HD1080, vec![]);
    assert_eq!(
        registry.register(bad).unwrap_err(),
        Error::InvalidCapability("capability must list at least one pixel format")
    );

    // Zero-dimension resolution.
    let bad = Capability::new(
        BackendKind::Cuda,
        Stage::Decode,
        Resolution::new(0, 1080),
        vec![PixelFormat::Nv12],
    );
    assert_eq!(
        registry.register(bad).unwrap_err(),
        Error::InvalidCapability("max_resolution must be positive on both axes")
    );

    // decode_resize on a non-decode stage.
    let bad = Capability::new(
        BackendKind::Cuda,
        Stage::Encode,
        Resolution::HD1080,
        vec![PixelFormat::Nv12],
    )
    .with_decode_resize(true);
    assert_eq!(
        registry.register(bad).unwrap_err(),
        Error::InvalidCapability("decode_resize is only meaningful on the Decode stage")
    );

    assert!(registry.is_empty());
}

#[test]
fn software_defaults_register_every_stage() {
    let registry = BackendRegistry::with_software_defaults();
    assert_eq!(registry.len(), Stage::ALL.len());
    for stage in Stage::ALL {
        let cap = registry
            .require(stage, BackendKind::Software)
            .expect("software default present for every stage");
        assert_eq!(cap.kind, BackendKind::Software);
        assert!(cap.supports_format(PixelFormat::Nv12));
    }
}

#[test]
fn for_stage_returns_only_matching_stage_in_deterministic_order() {
    let mut registry = BackendRegistry::with_software_defaults();
    registry.register(cuda_decode()).unwrap();
    registry
        .register(Capability::new(
            BackendKind::Vaapi,
            Stage::Decode,
            Resolution::HD1080,
            vec![PixelFormat::Nv12],
        ))
        .unwrap();

    let decoders = registry.for_stage(Stage::Decode);
    // software + cuda + vaapi.
    assert_eq!(decoders.len(), 3);
    assert!(decoders.iter().all(|c| c.stage == Stage::Decode));

    // Ordering is deterministic (by stable kind ordinal: Software, Cuda, Vaapi).
    let kinds: Vec<BackendKind> = decoders.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        vec![BackendKind::Software, BackendKind::Cuda, BackendKind::Vaapi]
    );

    // Composite stage has only the software default.
    assert_eq!(registry.for_stage(Stage::Composite).len(), 1);
}
