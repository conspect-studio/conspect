//! The **real** pure-Rust NV12 → JPEG encoder for the default (GPU-free) build.
//!
//! [`StubJpegEncoder`](crate::StubJpegEncoder) exists so the framing/transport
//! and refcount layers are testable with no codec; [`Nv12JpegEncoder`] is the
//! production default: it converts an NV12 thumbnail plane to packed `YCbCr` and
//! hands it to the dependency-free, `forbid(unsafe_code)` [`jpeg_encoder`] crate
//! (pinned to the `MIT OR Apache-2.0` 0.6 line, `simd` off — see `Cargo.toml`).
//!
//! ## Why `YCbCr`, not RGB
//!
//! Invariant #5 keeps frames in NV12 throughout the pipeline; we never
//! materialize a per-tile RGBA buffer. The JPEG container is itself a `YCbCr`
//! format, so the cheapest *and* most faithful path is to **upsample NV12 chroma
//! to packed `YCbCr` (3 B/px)** and let the JPEG encoder re-subsample to its own
//! 4:2:0 — no `YCbCr → RGB → YCbCr` round trip, no colour-matrix loss for the
//! preview thumbnail. (The brief's "YUV→RGB happens in-shader" rule is about the
//! *compositor*; the preview JPEG seam only re-packs the planes.)
//!
//! Geometry / quality validation is shared with the stub via
//! [`validate_nv12`](crate::framing) so callers get identical typed errors.
use jpeg_encoder::{ColorType, Encoder};

use crate::framing::{validate_nv12, JpegEncoder, JpegError};

/// The default chroma subsampling: 4:2:0 (`2x2`), matching the NV12 source and
/// keeping preview thumbnails small.
const PREVIEW_SAMPLING: jpeg_encoder::SamplingFactor = jpeg_encoder::SamplingFactor::R_4_2_0;

/// A real NV12 → JPEG encoder built on the pure-Rust [`jpeg_encoder`] crate.
///
/// Stateless and cheap to clone; hold one per preview tap (or one shared). It is
/// the production default behind the [`JpegEncoder`] trait, swappable for a
/// hardware path (`turbojpeg`/`VideoToolbox`) on the same seam later.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct Nv12JpegEncoder;

impl Nv12JpegEncoder {
    /// Build a default NV12 → `YCbCr` JPEG encoder.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl JpegEncoder for Nv12JpegEncoder {
    fn encode_nv12(
        &self,
        plane: &[u8],
        width: u32,
        height: u32,
        quality: u8,
    ) -> Result<Vec<u8>, JpegError> {
        validate_nv12(plane, width, height, quality)?;

        // Geometry is validated; `width`/`height` are even and non-zero and the
        // product fits a host buffer. JPEG dimensions are `u16`; a preview
        // thumbnail is always far under 65 535 px, but reject (rather than
        // truncate) anything that would not fit so we never silently mis-size.
        let (jw, jh) = u16::try_from(width)
            .ok()
            .zip(u16::try_from(height).ok())
            .ok_or(JpegError::OddDimensions { width, height })?;

        let packed = nv12_to_packed_ycbcr(plane, width, height)?;

        let mut out = Vec::new();
        let mut encoder = Encoder::new(&mut out, quality);
        encoder.set_sampling_factor(PREVIEW_SAMPLING);
        encoder
            .encode(&packed, jw, jh, ColorType::Ycbcr)
            .map_err(|_| JpegError::Encode)?;
        Ok(out)
    }
}

/// Convert an NV12 plane to packed `YCbCr` (3 bytes/pixel), upsampling the 4:2:0
/// chroma by nearest-neighbour (each chroma sample covers its `2x2` luma block).
///
/// NV12 layout: a `width*height` luma plane, then a `width*height/2` interleaved
/// `CbCr` plane of `width/2 * height/2` samples, two bytes (`Cb`,`Cr`) each, with
/// a row stride of `width` bytes. Total, index-safe, and `as`-free on the hot
/// path (every access is a checked `get`).
fn nv12_to_packed_ycbcr(plane: &[u8], width: u32, height: u32) -> Result<Vec<u8>, JpegError> {
    let w = usize::try_from(width).map_err(|_| JpegError::OddDimensions { width, height })?;
    let h = usize::try_from(height).map_err(|_| JpegError::OddDimensions { width, height })?;
    let luma_len = w.checked_mul(h).ok_or(JpegError::BufferTooSmall {
        have: plane.len(),
        need: usize::MAX,
        width,
        height,
    })?;
    // `split_at_checked` keeps this total: a caller that skipped `validate_nv12`
    // (callers in this crate never do) gets a typed error, never a panic.
    let (luma, chroma) = plane
        .split_at_checked(luma_len)
        .ok_or(JpegError::BufferTooSmall {
            have: plane.len(),
            need: luma_len,
            width,
            height,
        })?;
    // chroma row stride is `w` bytes (w/2 Cb + w/2 Cr interleaved).
    let chroma_stride = w;

    let mut packed = Vec::with_capacity(luma_len.saturating_mul(3));
    for y in 0..h {
        let cy = y / 2;
        for x in 0..w {
            let cx = x / 2;
            // Default to neutral grey if a (validated) index somehow misses,
            // rather than panicking on the hot preview path.
            let yv = luma.get(y.saturating_mul(w).saturating_add(x)).copied();
            let chroma_base = cy
                .saturating_mul(chroma_stride)
                .saturating_add(cx.saturating_mul(2));
            let cb = chroma.get(chroma_base).copied();
            let cr = chroma.get(chroma_base.saturating_add(1)).copied();
            packed.push(yv.unwrap_or(16));
            packed.push(cb.unwrap_or(128));
            packed.push(cr.unwrap_or(128));
        }
    }
    Ok(packed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn packs_nv12_to_ycbcr_with_chroma_upsample() {
        // 2x2 luma {10,20,30,40}, one chroma sample (Cb=100, Cr=200) covering all
        // four pixels.
        let plane = vec![10u8, 20, 30, 40, 100, 200];
        let packed = nv12_to_packed_ycbcr(&plane, 2, 2).unwrap();
        assert_eq!(packed.len(), 2 * 2 * 3);
        // Each pixel keeps its own luma; all share the single chroma sample.
        assert_eq!(&packed[0..3], &[10, 100, 200]);
        assert_eq!(&packed[3..6], &[20, 100, 200]);
        assert_eq!(&packed[6..9], &[30, 100, 200]);
        assert_eq!(&packed[9..12], &[40, 100, 200]);
    }

    #[test]
    fn rejects_short_buffer_before_packing() {
        let enc = Nv12JpegEncoder::new();
        assert!(matches!(
            enc.encode_nv12(&[0u8; 2], 4, 2, 80),
            Err(JpegError::BufferTooSmall { .. })
        ));
    }
}
