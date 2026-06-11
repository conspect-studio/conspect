//! Display buffer-strategy selection tests (DEV-B3 / ADR-0044 §2): the
//! per-hardware buffer-strategy decision — NV12-direct scanout vs the wgpu
//! NV12→XRGB render pass vs the CPU NV12→XRGB fallback — proven WITHOUT a GPU
//! against mock plane/canvas descriptors. This is the testable core of the
//! render path: the actual ADDFB2 import (drm/gbm) and the wgpu pass run only
//! on hardware, but *which* path is chosen is decided by pure data here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::display::strategy::{
    parse_in_formats_blob, plane_supports_nv12, select_buffer_strategy, BufferStrategy,
    CanvasDelivery, DrmFormat, PlaneFormatCaps, ScanoutCaps,
};

/// The Broadcom SAND128 column-tiled modifier (vc4 hardware-decoder native
/// tiling) — `DRM_FORMAT_MOD_BROADCOM_SAND128` with a 0-column-height payload.
const SAND128: u64 = 0x0700_0000_0000_0000;
/// `DRM_FORMAT_MOD_LINEAR` (no tiling).
const LINEAR: u64 = 0;

// ---------------------------------------------------------------------------
// DrmFormat — fourcc round-trips and the named constants
// ---------------------------------------------------------------------------

#[test]
fn drm_format_fourcc_round_trips() {
    // XR24 little-endian = the XRGB8888 scanout format.
    assert_eq!(DrmFormat::XRGB8888, DrmFormat::from_fourcc(*b"XR24"));
    // NV12 is its own fourcc.
    assert_eq!(DrmFormat::NV12, DrmFormat::from_fourcc(*b"NV12"));
    assert_eq!(DrmFormat::XRGB8888.fourcc(), u32::from_le_bytes(*b"XR24"));
    assert_eq!(DrmFormat::NV12.fourcc(), u32::from_le_bytes(*b"NV12"));
    assert_ne!(DrmFormat::XRGB8888, DrmFormat::NV12);
}

// ---------------------------------------------------------------------------
// plane_supports_nv12 — the plane-format gating
// ---------------------------------------------------------------------------

#[test]
fn plane_with_nv12_linear_supports_nv12_linear() {
    let plane = PlaneFormatCaps::new(vec![DrmFormat::XRGB8888, DrmFormat::NV12], vec![LINEAR]);
    // A plane that lists NV12 + the LINEAR modifier supports the linear NV12.
    assert_eq!(
        plane_supports_nv12(&plane, Some(LINEAR)),
        Some(DrmFormat::NV12)
    );
}

#[test]
fn plane_without_nv12_format_never_supports_nv12() {
    // The AMD DCE11 reality: RGB-only primaries, no NV12 format at all.
    let plane = PlaneFormatCaps::new(vec![DrmFormat::XRGB8888], vec![LINEAR]);
    assert_eq!(plane_supports_nv12(&plane, Some(LINEAR)), None);
    assert_eq!(plane_supports_nv12(&plane, None), None);
}

#[test]
fn plane_gates_nv12_on_the_requested_modifier() {
    // vc4: NV12 is listed, but SAND128 tiling is required for the decoder's
    // native buffer. A plane that lists NV12 only under LINEAR must reject a
    // SAND128 request (the dmabuf would be mis-tiled → green garbage, #5727).
    let plane = PlaneFormatCaps::new(vec![DrmFormat::NV12], vec![LINEAR]);
    assert_eq!(plane_supports_nv12(&plane, Some(SAND128)), None);
    // The same plane that *does* advertise SAND128 accepts it.
    let sand = PlaneFormatCaps::new(vec![DrmFormat::NV12], vec![LINEAR, SAND128]);
    assert_eq!(
        plane_supports_nv12(&sand, Some(SAND128)),
        Some(DrmFormat::NV12)
    );
}

#[test]
fn plane_with_no_advertised_modifiers_accepts_a_linear_request() {
    // Legacy drivers expose formats with no IN_FORMATS modifier blob; a
    // LINEAR (or modifier-agnostic `None`) request must still be honoured.
    let plane = PlaneFormatCaps::new(vec![DrmFormat::NV12], Vec::new());
    assert_eq!(plane_supports_nv12(&plane, None), Some(DrmFormat::NV12));
    assert_eq!(
        plane_supports_nv12(&plane, Some(LINEAR)),
        Some(DrmFormat::NV12)
    );
    // But a non-linear tiling request cannot be honoured without proof.
    assert_eq!(plane_supports_nv12(&plane, Some(SAND128)), None);
}

// ---------------------------------------------------------------------------
// select_buffer_strategy — the per-hardware decision (display-out.md §2 table)
// ---------------------------------------------------------------------------

#[test]
fn intel_gen9_nv12_canvas_chooses_zero_copy_direct_scanout() {
    // Intel Gen9+: NV12 on the primary plane + an NV12 (linear) canvas dmabuf
    // → 0 copies, 0 render passes.
    let caps = ScanoutCaps {
        plane: PlaneFormatCaps::new(vec![DrmFormat::XRGB8888, DrmFormat::NV12], vec![LINEAR]),
        canvas: CanvasDelivery::Dmabuf {
            format: DrmFormat::NV12,
            modifier: Some(LINEAR),
        },
        gpu_pass_available: true,
    };
    assert_eq!(
        select_buffer_strategy(&caps),
        BufferStrategy::Nv12Direct {
            format: DrmFormat::NV12,
            modifier: Some(LINEAR),
        }
    );
}

#[test]
fn vc4_sand_canvas_chooses_direct_scanout_with_the_sand_modifier() {
    // Raspberry Pi vc4: NV12 + SAND128 on the plane, and the V4L2 decoder hands
    // us a SAND-tiled NV12 dmabuf → 0 copies, 0 render passes, SAND modifier.
    let caps = ScanoutCaps {
        plane: PlaneFormatCaps::new(vec![DrmFormat::NV12], vec![LINEAR, SAND128]),
        canvas: CanvasDelivery::Dmabuf {
            format: DrmFormat::NV12,
            modifier: Some(SAND128),
        },
        gpu_pass_available: false,
    };
    assert_eq!(
        select_buffer_strategy(&caps),
        BufferStrategy::Nv12Direct {
            format: DrmFormat::NV12,
            modifier: Some(SAND128),
        }
    );
}

#[test]
fn amd_dce11_rgb_only_plane_with_gpu_chooses_the_wgpu_xrgb_pass() {
    // AMD DCE11: no NV12 scanout exists; with a GPU importer wired the one
    // wgpu NV12→XRGB pass is the path.
    let caps = ScanoutCaps {
        plane: PlaneFormatCaps::new(vec![DrmFormat::XRGB8888], vec![LINEAR]),
        canvas: CanvasDelivery::Dmabuf {
            format: DrmFormat::NV12,
            modifier: Some(LINEAR),
        },
        gpu_pass_available: true,
    };
    assert_eq!(select_buffer_strategy(&caps), BufferStrategy::WgpuXrgbPass);
}

#[test]
fn rgb_only_plane_without_gpu_falls_back_to_the_cpu_convert() {
    // The guaranteed default: an RGB-only plane and no wired GPU importer →
    // the portable CPU NV12→XRGB conversion (DEV-B1) carries the frame.
    let caps = ScanoutCaps {
        plane: PlaneFormatCaps::new(vec![DrmFormat::XRGB8888], vec![LINEAR]),
        canvas: CanvasDelivery::Dmabuf {
            format: DrmFormat::NV12,
            modifier: Some(LINEAR),
        },
        gpu_pass_available: false,
    };
    assert_eq!(
        select_buffer_strategy(&caps),
        BufferStrategy::CpuXrgbConvert
    );
}

#[test]
fn cpu_only_canvas_never_chooses_direct_scanout_even_on_an_nv12_plane() {
    // No dmabuf to import (the canvas is CPU planes): NV12-direct is
    // impossible regardless of plane caps; a GPU pass can't import CPU planes
    // either on this seam, so the CPU convert is the path.
    let caps = ScanoutCaps {
        plane: PlaneFormatCaps::new(vec![DrmFormat::XRGB8888, DrmFormat::NV12], vec![LINEAR]),
        canvas: CanvasDelivery::CpuPlanes,
        gpu_pass_available: true,
    };
    assert_eq!(
        select_buffer_strategy(&caps),
        BufferStrategy::CpuXrgbConvert
    );
}

#[test]
fn nv12_plane_but_modifier_mismatch_falls_back_not_direct() {
    // The plane offers NV12 only LINEAR, but the canvas dmabuf is SAND-tiled:
    // a direct flip of a mis-tiled buffer is the #5727 green-screen hazard, so
    // the selector must NOT choose direct — it falls to the CPU convert (no
    // GPU importer here).
    let caps = ScanoutCaps {
        plane: PlaneFormatCaps::new(vec![DrmFormat::NV12], vec![LINEAR]),
        canvas: CanvasDelivery::Dmabuf {
            format: DrmFormat::NV12,
            modifier: Some(SAND128),
        },
        gpu_pass_available: false,
    };
    assert_eq!(
        select_buffer_strategy(&caps),
        BufferStrategy::CpuXrgbConvert
    );
}

// ---------------------------------------------------------------------------
// parse_in_formats_blob — the kernel IN_FORMATS modifier blob → (formats,
// modifiers). Pure byte parsing, so it is tested here without a real plane.
// ---------------------------------------------------------------------------

/// Build a `struct drm_format_modifier_blob` payload (uapi `drm_mode.h`):
/// a 24-byte header, then the `u32` format fourccs, then 24-byte
/// `drm_format_modifier` records `{ formats: u64, offset: u32, pad: u32,
/// modifier: u64 }`.
fn build_in_formats_blob(formats: &[[u8; 4]], modifiers: &[(u64, u64)]) -> Vec<u8> {
    let header_len: u32 = 24;
    let formats_offset = header_len;
    let formats_len = u32::try_from(formats.len() * 4).unwrap();
    let modifiers_offset = formats_offset + formats_len;
    let mut blob = Vec::new();
    blob.extend_from_slice(&1u32.to_ne_bytes()); // version
    blob.extend_from_slice(&0u32.to_ne_bytes()); // flags
    blob.extend_from_slice(&u32::try_from(formats.len()).unwrap().to_ne_bytes());
    blob.extend_from_slice(&formats_offset.to_ne_bytes());
    blob.extend_from_slice(&u32::try_from(modifiers.len()).unwrap().to_ne_bytes());
    blob.extend_from_slice(&modifiers_offset.to_ne_bytes());
    for fmt in formats {
        blob.extend_from_slice(&u32::from_le_bytes(*fmt).to_ne_bytes());
    }
    for (format_mask, modifier) in modifiers {
        blob.extend_from_slice(&format_mask.to_ne_bytes()); // formats bitmask
        blob.extend_from_slice(&0u32.to_ne_bytes()); // offset (index base)
        blob.extend_from_slice(&0u32.to_ne_bytes()); // pad
        blob.extend_from_slice(&modifier.to_ne_bytes());
    }
    blob
}

#[test]
fn in_formats_blob_parses_formats_and_modifiers() {
    // Two formats (NV12, XR24); two modifiers each applying to both (bitmask
    // 0b11 = formats[0] and formats[1]).
    let blob = build_in_formats_blob(&[*b"NV12", *b"XR24"], &[(0b11, 0), (0b11, SAND128)]);
    let caps = parse_in_formats_blob(&blob).expect("valid blob parses");
    assert!(caps.has_format(DrmFormat::NV12));
    assert!(caps.has_format(DrmFormat::XRGB8888));
    // Both LINEAR and SAND128 are advertised.
    assert!(caps.accepts_modifier(Some(0)));
    assert!(caps.accepts_modifier(Some(SAND128)));
}

#[test]
fn truncated_in_formats_blob_is_a_none_not_a_panic() {
    // A short/garbage blob must never panic — return None so the caller falls
    // back to a linear-only assumption.
    assert!(parse_in_formats_blob(&[]).is_none());
    assert!(parse_in_formats_blob(&[0u8; 10]).is_none());
}

#[test]
fn direct_scanout_is_preferred_over_the_gpu_pass_when_both_are_possible() {
    // When the plane scans out NV12 AND a GPU importer is present, the
    // zero-copy direct path wins (it is strictly cheaper than a render pass).
    let caps = ScanoutCaps {
        plane: PlaneFormatCaps::new(vec![DrmFormat::XRGB8888, DrmFormat::NV12], vec![LINEAR]),
        canvas: CanvasDelivery::Dmabuf {
            format: DrmFormat::NV12,
            modifier: Some(LINEAR),
        },
        gpu_pass_available: true,
    };
    assert!(matches!(
        select_buffer_strategy(&caps),
        BufferStrategy::Nv12Direct { .. }
    ));
}
