//! Native-composite-backend admission (always compiled; pure, GPU-free).
//!
//! GPU-6 turns the compositor's vendor fast-path stubs (`cuda`/`vaapi`/`metal`)
//! into real native composite paths. The *real* native composite — a
//! device-resident scale + place + color-convert + linear-light blend on the
//! vendor's own engine — runs only on a GPU-tagged self-hosted runner (no
//! GPU/SDK on shared CI). What is pure host-side logic, and therefore lives
//! here, is the **admission decision**: can a native fast path serve a given
//! tile set at all, or must the engine fall back to the portable wgpu
//! compositor (which owns the explicit host copy)?
//!
//! The decision enforces three contracts before any GPU work is attempted:
//!
//! * **inv #5 (NV12-throughout):** a native path is admitted only for NV12/P010
//!   tiles. A single RGBA tile rejects it — RGBA is never materialized per tile;
//!   the YUV->RGB happens in-shader at tile size on the portable path instead.
//! * **zero-copy island (ADR-0004):** the native composite must run on the SAME
//!   device family the tiles are decoded on. A cross-vendor request (decode on
//!   one vendor, composite on another) would need a host copy across the
//!   boundary, which the native island cannot span — it falls back to wgpu.
//! * **graceful fallback:** every rejection names the portable wgpu fallback and
//!   a typed [`NativeRejection`] reason, so the engine degrades explicitly (and
//!   can log *why* a native path was not taken) rather than silently or by
//!   panicking.
//!
//! This module opens no device and performs no FFI; it is total and panic-free
//! (no `unwrap`/`expect`/indexing/`as`), so it compiles and runs in the default
//! pure-Rust build. The vendor `cuda`/`vaapi`/`metal` features gate the *real*
//! native composite implementation that consumes an [`NativeAdmission::Admit`];
//! this admission seam is feature-independent because the engine must be able to
//! reason about it even in a build without the native backend compiled.

use multiview_core::pixel::PixelFormat;
use multiview_core::traits::BackendKind;

/// The pixel formats of the tiles a composite pass will consume.
///
/// Only the per-tile *format* matters for admission (inv #5 cares about RGBA vs
/// NV12/P010, not geometry), so this is a compact list of [`PixelFormat`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileFormats {
    formats: Vec<PixelFormat>,
}

impl TileFormats {
    /// Build a tile-format set from an explicit list.
    #[must_use]
    pub fn new(formats: Vec<PixelFormat>) -> Self {
        Self { formats }
    }

    /// `count` tiles, all 8-bit NV12 (the canonical working format).
    #[must_use]
    pub fn all_nv12(count: usize) -> Self {
        Self {
            formats: vec![PixelFormat::Nv12; count],
        }
    }

    /// `count` tiles, all 10-bit P010.
    #[must_use]
    pub fn all_p010(count: usize) -> Self {
        Self {
            formats: vec![PixelFormat::P010; count],
        }
    }

    /// Set the tile at `index` to RGBA (a test/edge helper for the inv-#5 guard).
    /// Out-of-range indices are ignored — the set is left unchanged rather than
    /// panicking.
    pub fn set_rgba(&mut self, index: usize) {
        if let Some(slot) = self.formats.get_mut(index) {
            *slot = PixelFormat::Rgba;
        }
    }

    /// Whether every tile is a native working format (NV12 or P010) — i.e. no
    /// tile is RGBA. An empty set is trivially all-native.
    #[must_use]
    pub fn all_native_working_format(&self) -> bool {
        self.formats
            .iter()
            .all(|f| matches!(f, PixelFormat::Nv12 | PixelFormat::P010))
    }
}

/// The compositor a composite pass is dispatched to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompositeBackend {
    /// A vendor native fast path running on the named device family
    /// (`cuda`/`vaapi`/`metal`), keeping the whole composite island
    /// device-resident.
    Native(BackendKind),
    /// The portable wgpu compositor (the baseline, conventions §3). Owns the
    /// explicit host copy at any vendor/CPU boundary.
    Wgpu,
}

/// Why a native composite path was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeRejection {
    /// At least one tile is RGBA — admitting a native path would materialize
    /// RGBA per tile, violating inv #5.
    RgbaTile,
    /// The composite device family differs from the decode device family; a
    /// native island cannot span the vendor boundary (ADR-0004).
    CrossVendor,
    /// The decode source is software (host memory) — there is no native device
    /// island to composite on.
    NotADeviceIsland,
    /// The portable `wgpu` compositor was requested explicitly; this is not a
    /// vendor native path. (`Metal` is treated as a *native* vendor by
    /// [`is_native_vendor`], not as the portable backend — wgpu-on-Metal is a
    /// wgpu concern, not `BackendKind::Metal`.)
    PortableRequested,
}

/// The outcome of [`admit_native_composite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeAdmission {
    /// The native fast path is admissible; dispatch to the named backend.
    Admit(CompositeBackend),
    /// The native path was rejected; dispatch to `backend` (the portable wgpu
    /// compositor) and log `reason`.
    Fallback {
        /// The fallback compositor to dispatch to.
        backend: CompositeBackend,
        /// Why the native path was not taken.
        reason: NativeRejection,
    },
}

impl NativeAdmission {
    /// The concrete compositor to dispatch to, whatever the verdict — so the
    /// engine never has an unhandled state.
    #[must_use]
    pub fn backend(self) -> CompositeBackend {
        let (Self::Admit(backend) | Self::Fallback { backend, .. }) = self;
        backend
    }

    /// Whether a native vendor fast path was admitted.
    #[must_use]
    pub fn is_native(self) -> bool {
        matches!(self, Self::Admit(CompositeBackend::Native(_)))
    }
}

/// Whether `kind` is a vendor family that has a native composite fast path.
///
/// `Software`/`Wgpu` are not native vendors (software is host-side; wgpu is the
/// portable backend). `Qsv` shares VAAPI's media-block compositor on Linux, so
/// it is treated as a VAAPI-class native island here.
const fn is_native_vendor(kind: BackendKind) -> bool {
    matches!(
        kind,
        BackendKind::Cuda | BackendKind::Vaapi | BackendKind::Qsv | BackendKind::Metal
    )
}

/// Decide whether a native composite fast path can serve a tile set, or whether
/// the engine must fall back to the portable wgpu compositor.
///
/// `decode_kind` is the device family the tiles are decoded on; `composite_kind`
/// is the family the composite is requested on; `tiles` are the tile formats.
/// The checks run in this order (the first failure decides, with its typed
/// reason):
///
/// 1. The requested composite backend must be a native vendor — `Wgpu` (and
///    `Software`) request the portable path explicitly
///    ([`NativeRejection::PortableRequested`]).
/// 2. The decode source must be a device island, not software
///    ([`NativeRejection::NotADeviceIsland`]).
/// 3. Composite and decode must be the same vendor family — no cross-vendor
///    native island ([`NativeRejection::CrossVendor`]).
/// 4. Every tile must be a native working format; an RGBA tile is rejected for
///    inv #5 ([`NativeRejection::RgbaTile`]).
///
/// On success the native path on `composite_kind` is admitted; otherwise the
/// portable wgpu compositor is named with the reason.
#[must_use]
pub fn admit_native_composite(
    decode_kind: BackendKind,
    composite_kind: BackendKind,
    tiles: &TileFormats,
) -> NativeAdmission {
    let fallback = |reason| NativeAdmission::Fallback {
        backend: CompositeBackend::Wgpu,
        reason,
    };

    // 1. A portable (wgpu/software) composite request is never the native path.
    if !is_native_vendor(composite_kind) {
        return fallback(NativeRejection::PortableRequested);
    }
    // 2. Software-decoded tiles live in host memory — no native device island.
    if !is_native_vendor(decode_kind) {
        return fallback(NativeRejection::NotADeviceIsland);
    }
    // 3. The native island cannot span a vendor boundary (ADR-0004).
    if decode_kind != composite_kind {
        return fallback(NativeRejection::CrossVendor);
    }
    // 4. inv #5: a native path must never materialize RGBA per tile.
    if !tiles.all_native_working_format() {
        return fallback(NativeRejection::RgbaTile);
    }

    NativeAdmission::Admit(CompositeBackend::Native(composite_kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tile_set_is_all_native() {
        assert!(TileFormats::all_nv12(0).all_native_working_format());
    }

    #[test]
    fn p010_counts_as_a_native_working_format() {
        assert!(TileFormats::all_p010(3).all_native_working_format());
    }

    #[test]
    fn set_rgba_out_of_range_is_a_no_op_not_a_panic() {
        let mut f = TileFormats::all_nv12(2);
        f.set_rgba(99);
        assert!(f.all_native_working_format(), "no tile was changed");
    }

    #[test]
    fn qsv_is_treated_as_a_native_vendor() {
        assert!(is_native_vendor(BackendKind::Qsv));
        assert!(!is_native_vendor(BackendKind::Software));
        assert!(!is_native_vendor(BackendKind::Wgpu));
    }
}
