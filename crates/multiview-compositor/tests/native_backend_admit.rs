//! Native-composite-backend admission tests (GPU-free).
//!
//! GPU-6 turns the compositor's vendor fast-path stubs (`cuda`/`vaapi`/`metal`)
//! into real native composite paths. The *real* native composite runs only on a
//! GPU-tagged self-hosted runner (no GPU/SDK here), but the **decision** of
//! whether a native fast path can serve a given tile set — and the inv-#5 guard
//! that it never materializes RGBA — is pure host-side logic, so it is pinned
//! here and runs on shared CI.
//!
//! The admission seam upholds:
//! * **inv #5 (NV12-throughout):** a native path is admitted only for NV12/P010
//!   tiles; any RGBA tile is rejected (RGBA is never materialized per tile).
//! * **zero-copy island (ADR-0004):** the native composite must run on the SAME
//!   device family the tiles are decoded on; a cross-vendor request is rejected
//!   (it would need a host copy, which the wgpu/CPU fallback owns explicitly).
//! * **graceful fallback:** when a native path cannot be admitted the seam names
//!   the portable wgpu fallback rather than failing.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::native::{
    admit_native_composite, CompositeBackend, NativeAdmission, NativeRejection, TileFormats,
};
use multiview_core::traits::BackendKind;

#[test]
fn nv12_tiles_on_a_matching_device_admit_the_native_path() {
    // All tiles NV12, decoded on CUDA, composite requested on CUDA: the native
    // CUDA fast path is admissible (whole island on one device).
    let admission = admit_native_composite(
        BackendKind::Cuda,
        BackendKind::Cuda,
        &TileFormats::all_nv12(9),
    );
    assert_eq!(
        admission,
        NativeAdmission::Admit(CompositeBackend::Native(BackendKind::Cuda))
    );
}

#[test]
fn p010_tiles_still_admit_the_native_path() {
    // 10-bit P010 is a native working format too (inv #5); it must not force a
    // fallback.
    let admission = admit_native_composite(
        BackendKind::Vaapi,
        BackendKind::Vaapi,
        &TileFormats::all_p010(4),
    );
    assert_eq!(
        admission,
        NativeAdmission::Admit(CompositeBackend::Native(BackendKind::Vaapi))
    );
}

#[test]
fn an_rgba_tile_rejects_the_native_path_for_inv5() {
    // A single RGBA tile must reject the native path: inv #5 forbids
    // materializing RGBA per tile. The seam reports WHY and names the wgpu
    // fallback so the caller degrades explicitly rather than silently.
    let mut formats = TileFormats::all_nv12(9);
    formats.set_rgba(3);
    let admission = admit_native_composite(BackendKind::Cuda, BackendKind::Cuda, &formats);
    assert_eq!(
        admission,
        NativeAdmission::Fallback {
            backend: CompositeBackend::Wgpu,
            reason: NativeRejection::RgbaTile,
        }
    );
}

#[test]
fn a_cross_vendor_request_falls_back_to_wgpu() {
    // Tiles decoded on CUDA but composite requested on VAAPI would need a host
    // copy across the vendor boundary — the native zero-copy island cannot span
    // it (ADR-0004). The seam falls back to the portable wgpu compositor, which
    // owns the explicit copy.
    let admission = admit_native_composite(
        BackendKind::Cuda,
        BackendKind::Vaapi,
        &TileFormats::all_nv12(4),
    );
    assert_eq!(
        admission,
        NativeAdmission::Fallback {
            backend: CompositeBackend::Wgpu,
            reason: NativeRejection::CrossVendor,
        }
    );
}

#[test]
fn a_software_decode_source_falls_back_to_wgpu() {
    // Software-decoded tiles live in host memory; there is no native device
    // island to composite on, so the seam falls back to wgpu (which uploads).
    let admission = admit_native_composite(
        BackendKind::Software,
        BackendKind::Cuda,
        &TileFormats::all_nv12(2),
    );
    assert_eq!(
        admission,
        NativeAdmission::Fallback {
            backend: CompositeBackend::Wgpu,
            reason: NativeRejection::NotADeviceIsland,
        }
    );
}

#[test]
fn the_portable_wgpu_request_is_not_a_native_path() {
    // Requesting wgpu/metal-as-portable explicitly is never the vendor native
    // path; it is the portable compositor and is returned as such with no
    // rejection reason.
    let admission = admit_native_composite(
        BackendKind::Cuda,
        BackendKind::Wgpu,
        &TileFormats::all_nv12(4),
    );
    assert_eq!(
        admission,
        NativeAdmission::Fallback {
            backend: CompositeBackend::Wgpu,
            reason: NativeRejection::PortableRequested,
        }
    );
}

#[test]
fn an_empty_tile_set_admits_trivially_on_a_matching_device() {
    // No tiles yet (a just-started canvas). There is nothing to violate inv #5,
    // so a matching-device native path is admissible.
    let admission = admit_native_composite(
        BackendKind::Cuda,
        BackendKind::Cuda,
        &TileFormats::all_nv12(0),
    );
    assert_eq!(
        admission,
        NativeAdmission::Admit(CompositeBackend::Native(BackendKind::Cuda))
    );
}

#[test]
fn admission_reports_a_renderable_backend_kind_either_way() {
    // Whatever the verdict, the seam yields a concrete CompositeBackend the
    // engine can dispatch to — never an unhandled state.
    let admit = admit_native_composite(
        BackendKind::Cuda,
        BackendKind::Cuda,
        &TileFormats::all_nv12(4),
    );
    assert!(matches!(
        admit.backend(),
        CompositeBackend::Native(BackendKind::Cuda)
    ));

    let fallback = admit_native_composite(
        BackendKind::Software,
        BackendKind::Cuda,
        &TileFormats::all_nv12(4),
    );
    assert_eq!(fallback.backend(), CompositeBackend::Wgpu);
}
