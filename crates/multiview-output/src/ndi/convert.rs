//! OUT-4: the pure canvas→NDI host-copy seam.
//!
//! NDI sends frames from **host memory** (never GPU surfaces; ADR-0004), so the
//! one GPU→host copy of the composited canvas lives at this boundary. The canvas
//! is NV12 (4:2:0 semi-planar) internally; the low-latency NDI default
//! (`color_format_fastest`, core-engine §9.2/§10) is **UYVY** (8-bit 4:2:2
//! packed). This module converts NV12 → UYVY and builds the validated
//! [`NdiVideoFrame`] send-descriptor an [`super::output::NdiOutput`] pushes.
//!
//! Everything here is pure, SDK-free, panic-free logic — no `unwrap`/`expect`,
//! no `as` casts, **checked indexing only** (mirroring the compositor's
//! plane-copy discipline). It is fully unit-tested in CI under the `ndi` feature
//! without any proprietary runtime; the actual send is the live-only concern.

use super::api::{NdiFourCc, NdiSendError, NdiVideoFrame};

/// A borrowed NV12 (4:2:0 semi-planar) host canvas view: a tightly-packed
/// `width * height` Y plane followed by a `width * (height / 2)` interleaved
/// Cb,Cr plane — the CPU-reference layout the compositor produces.
///
/// Borrows the planes for the duration of the conversion; it does not own pixel
/// memory. Construction validates the geometry (even, positive dimensions) and
/// the plane lengths so a malformed canvas is a *typed refusal*, never a panic
/// or an out-of-bounds read downstream.
#[derive(Debug, Clone, Copy)]
pub struct Nv12Canvas<'a> {
    width: u32,
    height: u32,
    y_plane: &'a [u8],
    uv_plane: &'a [u8],
}

impl<'a> Nv12Canvas<'a> {
    /// Build an NV12 canvas view from explicit planes.
    ///
    /// `width`/`height` must be positive and even (4:2:0 chroma subsampling);
    /// `y_plane` must be exactly `width * height` bytes and `uv_plane` exactly
    /// `width * height / 2` bytes.
    ///
    /// # Errors
    /// [`NdiSendError::InvalidFrame`] if the dimensions are zero/odd or a plane
    /// length does not match the geometry.
    pub fn new(
        width: u32,
        height: u32,
        y_plane: &'a [u8],
        uv_plane: &'a [u8],
    ) -> Result<Self, NdiSendError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(NdiSendError::InvalidFrame {
                detail: format!("NV12 dimensions must be positive and even (got {width}x{height})"),
            });
        }
        // Widening conversions on every target we support (64-bit Linux + macOS);
        // `u64` keeps the length math cast-free and overflow-free.
        let y_len = u64::from(width).saturating_mul(u64::from(height));
        let uv_len = y_len / 2;
        let have_y = u64::try_from(y_plane.len()).unwrap_or(u64::MAX);
        let have_uv = u64::try_from(uv_plane.len()).unwrap_or(u64::MAX);
        if have_y != y_len {
            return Err(NdiSendError::InvalidFrame {
                detail: format!("Y plane length {} != expected {y_len}", y_plane.len()),
            });
        }
        if have_uv != uv_len {
            return Err(NdiSendError::InvalidFrame {
                detail: format!("UV plane length {} != expected {uv_len}", uv_plane.len()),
            });
        }
        Ok(Self {
            width,
            height,
            y_plane,
            uv_plane,
        })
    }

    /// Canvas width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Canvas height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Borrow the tightly-packed Y plane (`width * height` bytes).
    #[must_use]
    pub const fn y_plane(&self) -> &[u8] {
        self.y_plane
    }

    /// Borrow the interleaved Cb,Cr plane (`width * height / 2` bytes).
    #[must_use]
    pub const fn uv_plane(&self) -> &[u8] {
        self.uv_plane
    }

    /// The number of bytes a packed UYVY buffer for this canvas occupies
    /// (`width * 2 * height`).
    #[must_use]
    pub fn uyvy_len(&self) -> u64 {
        u64::from(self.width)
            .saturating_mul(2)
            .saturating_mul(u64::from(self.height))
    }

    /// The UYVY line stride in bytes for this canvas (`width * 2`).
    #[must_use]
    pub fn uyvy_stride(&self) -> u32 {
        self.width.saturating_mul(2)
    }

    /// Build the validated [`NdiVideoFrame`] send-descriptor for an already-packed
    /// UYVY `buffer` (typically produced by [`nv12_to_uyvy`]).
    ///
    /// `timecode` is in NDI 100 ns units and is expected to already be re-stamped
    /// from the tick counter (invariant #3) by the caller — never raw input PTS.
    /// `frame_rate_n`/`frame_rate_d` are the exact rational cadence (e.g.
    /// `30000/1001` for NTSC; never float fps). The descriptor is run through
    /// [`NdiVideoFrame::validate`] so a mismatched buffer or zero denominator is a
    /// typed refusal, not a downstream UB read.
    ///
    /// # Errors
    /// [`NdiSendError::InvalidFrame`] if the descriptor is internally
    /// inconsistent (buffer too short for `stride * height`, zero denominator).
    pub fn to_uyvy_frame<'b>(
        &self,
        timecode: i64,
        frame_rate_n: u32,
        frame_rate_d: u32,
        buffer: &'b [u8],
    ) -> Result<NdiVideoFrame<'b>, NdiSendError> {
        let frame = NdiVideoFrame {
            width: self.width,
            height: self.height,
            stride: self.uyvy_stride(),
            fourcc: NdiFourCc::Uyvy,
            frame_rate_n,
            frame_rate_d,
            timecode,
            data: buffer,
        };
        frame.validate()?;
        Ok(frame)
    }
}

/// Convert an NV12 (4:2:0 semi-planar) canvas into a packed UYVY (4:2:2) host
/// buffer of `width * 2 * height` bytes.
///
/// UYVY packs two horizontal pixels per 4-byte group in `U, Y0, V, Y1` order.
/// 4:2:0 chroma (one Cb,Cr per 2x2 luma block) is up-sampled to 4:2:2 by
/// **vertical replication**: both luma rows of a 2x2 block reuse the block's
/// single chroma pair (nearest, no interpolation — the GPU fast path does proper
/// chroma siting; this CPU-reference boundary mirrors the compositor's chroma
/// replication). Horizontal chroma is already at 4:2:2 resolution after the pack.
///
/// Pure and panic-free: all plane access is checked (`.get`), so an undersized
/// plane (which [`Nv12Canvas::new`] already rejects) would yield a neutral 0
/// sample rather than an out-of-bounds read.
#[must_use]
pub fn nv12_to_uyvy(canvas: &Nv12Canvas<'_>) -> Vec<u8> {
    // Geometry as `usize` for indexing math. `try_from` is infallible here for
    // the validated even/positive dimensions on every supported (64-bit) target;
    // a (theoretically impossible) failure degrades to an empty buffer rather
    // than panicking.
    let (Ok(width), Ok(height)) = (
        usize::try_from(canvas.width),
        usize::try_from(canvas.height),
    ) else {
        return Vec::new();
    };
    let luma = canvas.y_plane;
    let chroma = canvas.uv_plane;
    // Chroma row stride in the interleaved plane: `width` bytes (width/2 Cb,Cr
    // pairs).
    let chroma_row_pairs = width / 2;

    let out_len = width.saturating_mul(2).saturating_mul(height);
    let mut out = Vec::with_capacity(out_len);

    for row in 0..height {
        let luma_row = row.saturating_mul(width);
        let chroma_row = row / 2;
        let chroma_row_base = chroma_row
            .saturating_mul(chroma_row_pairs)
            .saturating_mul(2);
        // Walk the row two luma pixels at a time (one UYVY group per pair).
        let mut col = 0usize;
        while col + 1 < width {
            let chroma_pair = col / 2;
            let chroma_index = chroma_row_base.saturating_add(chroma_pair.saturating_mul(2));
            // Neutral fallbacks (cb/cr = 128, luma = 16) are unreachable for a
            // canvas validated by `Nv12Canvas::new`; they keep the access checked
            // and panic-free regardless.
            let cb = chroma.get(chroma_index).copied().unwrap_or(128);
            let cr = chroma
                .get(chroma_index.saturating_add(1))
                .copied()
                .unwrap_or(128);
            let luma0 = luma
                .get(luma_row.saturating_add(col))
                .copied()
                .unwrap_or(16);
            let luma1 = luma
                .get(luma_row.saturating_add(col).saturating_add(1))
                .copied()
                .unwrap_or(16);
            out.push(cb);
            out.push(luma0);
            out.push(cr);
            out.push(luma1);
            col = col.saturating_add(2);
        }
    }
    out
}
