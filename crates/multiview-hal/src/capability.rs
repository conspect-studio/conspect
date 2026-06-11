//! Capability descriptors: what a `(backend kind, stage)` pair can do.
//!
//! A [`Capability`] records, per backend and pipeline [`Stage`], the maximum
//! resolution it supports, the pixel formats it accepts, and — for decode —
//! whether it can resize *during* decode (the NVDEC fused-resize lever from
//! [efficiency §1](../../../docs/research/efficiency.md)). These are the static
//! priors the [`crate::planner::Planner`] consumes; the actual hardware probing
//! lives behind off-by-default features in [`crate::probe`] and merely produces
//! (or fails to produce) these descriptors.
use core::cmp::Ordering;
use multiview_core::pixel::PixelFormat;
use multiview_core::traits::BackendKind;

use crate::error::{Error, Result};

/// A pipeline stage that a backend can implement.
///
/// Distinct from [`BackendKind`] (the *vendor*); a single backend kind may
/// implement several stages (e.g. CUDA does decode, composite, and — via
/// NVENC — encode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Stage {
    /// Coded packets -> frames (NVDEC / VAAPI / `VideoToolbox` / software).
    Decode,
    /// Scale + place + color-convert + blend the canvas (the GPU compositor).
    Composite,
    /// Canvas frames -> coded packets (NVENC / VAAPI / `VideoToolbox` / x264).
    Encode,
}

impl Stage {
    /// All stages, in pipeline order.
    pub const ALL: [Stage; 3] = [Stage::Decode, Stage::Composite, Stage::Encode];
}

/// A pixel resolution (width x height), used as a capability ceiling and to
/// size per-frame cost in megapixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Resolution {
    /// Width in pixels (must be `> 0`).
    pub width: u32,
    /// Height in pixels (must be `> 0`).
    pub height: u32,
}

impl Resolution {
    /// 1280x720.
    pub const HD720: Self = Self::new(1280, 720);
    /// 1920x1080.
    pub const HD1080: Self = Self::new(1920, 1080);
    /// 3840x2160.
    pub const UHD4K: Self = Self::new(3840, 2160);

    /// Construct a resolution.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Pixel count (`width * height`) as `u64` (never overflows for `u32 *
    /// u32`).
    #[must_use]
    pub fn pixels(self) -> u64 {
        // `u32 -> u64` via `u64::from` is lossless and avoids an `as` cast;
        // the product of two `u32` values always fits in `u64`.
        u64::from(self.width) * u64::from(self.height)
    }

    /// Pixel count expressed in **megapixels** (`pixels / 1_000_000`), as an
    /// `f64`.
    ///
    /// Megapixels is the cost unit throughout the planner (invariant #6 budgets
    /// decode in megapixels/sec, not stream count: a 4K tile costs ~9x a 720p
    /// tile).
    #[must_use]
    pub fn megapixels(self) -> f64 {
        // Pixel counts for real resolutions are far below 2^53, so the
        // u64 -> f64 widening is lossless here.
        let pixels = self.pixels();
        f64_from_u64(pixels) / 1_000_000.0
    }

    /// Whether this resolution fits within `ceiling` on both axes.
    #[must_use]
    pub const fn fits_within(self, ceiling: Resolution) -> bool {
        self.width <= ceiling.width && self.height <= ceiling.height
    }
}

impl PartialOrd for Resolution {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Resolution {
    /// Order by pixel count (area), tie-broken by width then height, so the
    /// derived ordering is a total order suitable for picking "the largest
    /// consuming tile".
    fn cmp(&self, other: &Self) -> Ordering {
        self.pixels()
            .cmp(&other.pixels())
            .then(self.width.cmp(&other.width))
            .then(self.height.cmp(&other.height))
    }
}

/// What a `(backend kind, stage)` pair can do.
///
/// Construct with the typed builder methods; [`Capability::validate`] (called
/// by the registry on insert) rejects structurally impossible descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    /// The backend kind this capability describes.
    pub kind: BackendKind,
    /// The pipeline stage this capability describes.
    pub stage: Stage,
    /// Maximum supported resolution (inclusive ceiling on both axes).
    pub max_resolution: Resolution,
    /// Pixel formats this backend accepts at this stage (non-empty).
    pub formats: Vec<PixelFormat>,
    /// Decode-only: whether the backend can resize *during* decode (the NVDEC
    /// fused-resize lever). Meaningless and always `false` for other stages.
    pub decode_resize: bool,
}

impl Capability {
    /// Build a capability descriptor.
    ///
    /// `formats` must be non-empty; [`Self::validate`] enforces that.
    #[must_use]
    pub fn new(
        kind: BackendKind,
        stage: Stage,
        max_resolution: Resolution,
        formats: Vec<PixelFormat>,
    ) -> Self {
        Self {
            kind,
            stage,
            max_resolution,
            formats,
            decode_resize: false,
        }
    }

    /// Builder: mark this (decode) capability as supporting fused decode-time
    /// resize.
    #[must_use]
    pub fn with_decode_resize(mut self, resize: bool) -> Self {
        self.decode_resize = resize;
        self
    }

    /// Whether this capability accepts the given pixel `format`.
    #[must_use]
    pub fn supports_format(&self, format: PixelFormat) -> bool {
        self.formats.contains(&format)
    }

    /// Whether this capability can handle a tile at `resolution` in `format`.
    #[must_use]
    pub fn supports(&self, resolution: Resolution, format: PixelFormat) -> bool {
        resolution.fits_within(self.max_resolution) && self.supports_format(format)
    }

    /// Validate structural invariants of this descriptor.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCapability`] if the max resolution has a zero
    /// dimension, the format list is empty, or `decode_resize` is set on a
    /// non-decode stage.
    pub fn validate(&self) -> Result<()> {
        if self.max_resolution.width == 0 || self.max_resolution.height == 0 {
            return Err(Error::InvalidCapability(
                "max_resolution must be positive on both axes",
            ));
        }
        if self.formats.is_empty() {
            return Err(Error::InvalidCapability(
                "capability must list at least one pixel format",
            ));
        }
        if self.decode_resize && self.stage != Stage::Decode {
            return Err(Error::InvalidCapability(
                "decode_resize is only meaningful on the Decode stage",
            ));
        }
        Ok(())
    }
}

/// Lossless `u64 -> f64` widening for values known to be below `2^53`.
///
/// Used only for pixel counts (`<= 3840*2160` for realistic tiles, far below
/// the `f64` integer-exactness bound), keeping the crate free of `as` casts.
fn f64_from_u64(value: u64) -> f64 {
    u32::try_from(value).map_or_else(
        |_| {
            // Split into high/low 32-bit halves; both widen losslessly and the
            // recombination is exact for any value < 2^53 (our domain).
            let high = (value >> 32) & 0xFFFF_FFFF;
            let low = value & 0xFFFF_FFFF;
            let high = u32::try_from(high).map_or(f64::INFINITY, f64::from);
            let low = u32::try_from(low).map_or(f64::INFINITY, f64::from);
            high * 4_294_967_296.0 + low
        },
        f64::from,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]
    use super::*;

    #[test]
    fn pixels_and_megapixels() {
        assert_eq!(Resolution::HD1080.pixels(), 2_073_600);
        assert!((Resolution::HD1080.megapixels() - 2.0736).abs() < 1e-12);
        assert!((Resolution::UHD4K.megapixels() - 8.2944).abs() < 1e-12);
    }

    #[test]
    fn resolution_orders_by_area() {
        assert!(Resolution::HD720 < Resolution::HD1080);
        assert!(Resolution::HD1080 < Resolution::UHD4K);
        // Equal area, ordered by width then height (total order).
        let a = Resolution::new(100, 200);
        let b = Resolution::new(200, 100);
        assert_eq!(a.pixels(), b.pixels());
        assert!(a < b);
    }

    #[test]
    fn fits_within_is_inclusive_on_both_axes() {
        assert!(Resolution::HD1080.fits_within(Resolution::HD1080));
        assert!(Resolution::HD720.fits_within(Resolution::HD1080));
        assert!(!Resolution::UHD4K.fits_within(Resolution::HD1080));
        // A tall-but-narrow tile that exceeds only the height must not fit.
        assert!(!Resolution::new(100, 5000).fits_within(Resolution::HD1080));
    }

    #[test]
    fn supports_requires_both_resolution_and_format() {
        let cap = Capability::new(
            BackendKind::Cuda,
            Stage::Decode,
            Resolution::HD1080,
            vec![PixelFormat::Nv12],
        );
        assert!(cap.supports(Resolution::HD720, PixelFormat::Nv12));
        // Over the resolution ceiling.
        assert!(!cap.supports(Resolution::UHD4K, PixelFormat::Nv12));
        // Unsupported format.
        assert!(!cap.supports(Resolution::HD720, PixelFormat::Rgba));
    }

    #[test]
    fn f64_from_u64_is_exact_for_large_values() {
        // A value above u32::MAX exercises the split-half path.
        let v: u64 = 5_000_000_000;
        assert_eq!(f64_from_u64(v), 5_000_000_000.0);
    }
}
