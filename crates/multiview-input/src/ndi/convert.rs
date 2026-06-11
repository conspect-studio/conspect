//! IN-3: the pure NDI-received-frame → NV12 host conversion seam.
//!
//! NDI delivers frames in **host memory** (never GPU surfaces; ADR-0004), so the
//! one host-side copy of a received frame lives at this boundary. NDI's
//! low-latency default `FourCC` is **UYVY** (8-bit 4:2:2 packed `Y'CbCr`); BGRA
//! (8-bit, with alpha) is used for keying/overlay sources. The Multiview ingest
//! canvas is **NV12** (4:2:0 semi-planar) internally (invariant #5), so this
//! module converts a received [`ReceivedVideoFrame`] into a [`HostNv12`] the
//! ingest pump publishes.
//!
//! ## Colour handling (honest scope)
//!
//! - **UYVY → NV12** is a pure *repack*: UYVY is **already** `Y'CbCr`, so no colour
//!   math happens here. The 4:2:2 → 4:2:0 step averages the two vertically-stacked
//!   chroma rows of each 2x2 block into the single NV12 chroma row (box filter,
//!   the inverse of the OUT-4 NV12→UYVY vertical-replication step). The samples
//!   stay in their source `Y'CbCr` space and are *tagged* `BT.709 limited`
//!   ([`ColorInfo::default`]); the compositor runs the full fixed colour pipeline
//!   (invariant #8) per tile against that tag.
//! - **BGRA → NV12** applies the **BT.709 limited-range** R'G'B' → `Y'CbCr` integer
//!   matrix (the canvas's tagged matrix), producing NV12 in exactly the space the
//!   tile is tagged with. The compositor's per-tile EOTF/primaries/OETF chain
//!   (invariant #8) then operates on the tagged result; doing the full
//!   colour-managed convert here would require pulling the compositor into
//!   `multiview-input` (wrong dependency direction), so the boundary is a straight
//!   matrix and the colour pipeline stays where it belongs.
//!
//! Everything here is pure, SDK-free, panic-free logic — no `unwrap`/`expect`, no
//! `as` casts, **checked indexing only** (mirroring the compositor's plane-copy
//! discipline). It is fully unit-tested in CI under the `ndi` feature without any
//! proprietary runtime; the actual receive is the live-only concern.

use multiview_core::color::ColorInfo;

use super::receiver::NdiRecvFourCc;

/// Why a received NDI frame could not be accepted or converted.
///
/// Every variant is a *typed refusal* — a malformed frame is reported, never a
/// panic or an out-of-bounds read. `#[non_exhaustive]` so new refusals are
/// additive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NdiConvertError {
    /// The frame geometry was zero, odd, or otherwise unusable for 4:2:0 NV12
    /// (which requires positive, even width and height).
    #[error("invalid NDI frame geometry: {detail}")]
    InvalidGeometry {
        /// Human-readable detail.
        detail: String,
    },
    /// The received buffer is shorter than `stride * height` (or the stride is
    /// narrower than the packed row), so the declared geometry cannot be read.
    #[error("NDI frame buffer too short: {detail}")]
    BufferTooShort {
        /// Human-readable detail.
        detail: String,
    },
    /// The frame's `FourCC` is not one this converter handles (e.g. P216, which
    /// is a later quality path).
    #[error("unsupported NDI FourCC for NV12 conversion: {0:?}")]
    UnsupportedFourCc(NdiRecvFourCc),
}

/// A borrowed-by-value received NDI video frame in host memory.
///
/// Owns the received pixel bytes plus the geometry and packing (`FourCC` +
/// `stride`) needed to read them. Construction validates the geometry (positive,
/// even) and that the buffer covers `stride * height`, so a malformed received
/// frame is a *typed refusal* at the boundary, never an out-of-bounds read in the
/// converter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedVideoFrame {
    width: u32,
    height: u32,
    fourcc: NdiRecvFourCc,
    stride: u32,
    /// The NDI per-frame timecode in 100 ns units (the producer's raw PTS). NDI
    /// timecodes are a 64-bit monotonic value; a negative `NDIlib_recv` synthesize
    /// sentinel is normalised to `None` by the receiver before it reaches here.
    timecode_100ns: Option<i64>,
    data: Vec<u8>,
}

impl ReceivedVideoFrame {
    /// Build a received frame from explicit geometry, packing, and host bytes,
    /// with no timecode (the producer's genpts fallback supplies one).
    ///
    /// `width`/`height` must be positive and even (4:2:0 target); `stride` must be
    /// at least the packed row width for the `FourCC`; `data` must be at least
    /// `stride * height` bytes.
    ///
    /// # Errors
    /// [`NdiConvertError::InvalidGeometry`] for zero/odd dimensions or a stride
    /// narrower than the packed row; [`NdiConvertError::BufferTooShort`] when the
    /// buffer does not cover `stride * height`.
    pub fn new(
        width: u32,
        height: u32,
        fourcc: NdiRecvFourCc,
        stride: u32,
        data: Vec<u8>,
    ) -> Result<Self, NdiConvertError> {
        Self::with_timecode(width, height, fourcc, stride, None, data)
    }

    /// As [`ReceivedVideoFrame::new`], carrying the NDI 100 ns timecode as the raw
    /// PTS the producer rebases onto the internal nanosecond timeline (invariant
    /// #3).
    ///
    /// # Errors
    /// See [`ReceivedVideoFrame::new`].
    pub fn with_timecode(
        width: u32,
        height: u32,
        fourcc: NdiRecvFourCc,
        stride: u32,
        timecode_100ns: Option<i64>,
        data: Vec<u8>,
    ) -> Result<Self, NdiConvertError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(NdiConvertError::InvalidGeometry {
                detail: format!("dimensions must be positive and even (got {width}x{height})"),
            });
        }
        let packed_row = match fourcc {
            // UYVY: 2 bytes per pixel (4:2:2 packed).
            NdiRecvFourCc::Uyvy => u64::from(width).saturating_mul(2),
            // BGRA: 4 bytes per pixel.
            NdiRecvFourCc::Bgra => u64::from(width).saturating_mul(4),
        };
        let stride64 = u64::from(stride);
        if stride64 < packed_row {
            return Err(NdiConvertError::InvalidGeometry {
                detail: format!("stride {stride} narrower than packed row {packed_row}"),
            });
        }
        let needed = stride64.saturating_mul(u64::from(height));
        // `usize`→`u64` is widening on every target we support (64-bit Linux +
        // macOS); `try_from` keeps it cast-free.
        let have = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if have < needed {
            return Err(NdiConvertError::BufferTooShort {
                detail: format!("buffer {} bytes < required {needed}", data.len()),
            });
        }
        Ok(Self {
            width,
            height,
            fourcc,
            stride,
            timecode_100ns,
            data,
        })
    }

    /// Frame width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The received `FourCC` packing.
    #[must_use]
    pub const fn fourcc(&self) -> NdiRecvFourCc {
        self.fourcc
    }

    /// The line stride (bytes per row) of the received buffer.
    #[must_use]
    pub const fn stride(&self) -> u32 {
        self.stride
    }

    /// The NDI 100 ns timecode (raw PTS), or `None` for the genpts fallback.
    #[must_use]
    pub const fn timecode_100ns(&self) -> Option<i64> {
        self.timecode_100ns
    }

    /// Borrow the received host bytes.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// An owned NV12 host frame: a tightly-packed `width * height` Y plane followed by
/// a `width * height / 2` interleaved Cb,Cr plane (4:2:0 semi-planar), plus the
/// resolved [`ColorInfo`] the samples are tagged with.
///
/// This is the CPU-reference container the ingest pump carries as
/// [`crate::source::ProducedFrame::pixels`] (Y plane then UV plane, concatenated)
/// — the same NV12 layout the compositor samples per tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostNv12 {
    width: u32,
    height: u32,
    y_plane: Vec<u8>,
    uv_plane: Vec<u8>,
    color: ColorInfo,
}

impl HostNv12 {
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
    pub fn y_plane(&self) -> &[u8] {
        &self.y_plane
    }

    /// Borrow the interleaved Cb,Cr plane (`width * height / 2` bytes).
    #[must_use]
    pub fn uv_plane(&self) -> &[u8] {
        &self.uv_plane
    }

    /// The resolved colour these samples are tagged with.
    #[must_use]
    pub const fn color(&self) -> ColorInfo {
        self.color
    }

    /// The concatenated NV12 host bytes (`Y` plane then `UV` plane), as carried in
    /// [`crate::source::ProducedFrame::pixels`].
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        let mut bytes = self.y_plane;
        bytes.extend_from_slice(&self.uv_plane);
        bytes
    }
}

/// Compute `(width, height)` as `usize` for indexing math, or `None` if the
/// (validated even/positive) geometry somehow overflows `usize` (impossible on a
/// 64-bit target — degrades to a typed refusal rather than a panic).
fn dims_usize(frame: &ReceivedVideoFrame) -> Option<(usize, usize)> {
    let w = usize::try_from(frame.width).ok()?;
    let h = usize::try_from(frame.height).ok()?;
    Some((w, h))
}

/// Convert a UYVY (8-bit 4:2:2 packed `Y'CbCr`) received frame into a [`HostNv12`].
///
/// UYVY packs two horizontal pixels per 4-byte group in `U, Y0, V, Y1` order.
/// This is a pure **repack** — the samples are already `Y'CbCr`, so no colour math
/// runs. Luma is taken verbatim. 4:2:2 → 4:2:0 averages the two vertically-stacked
/// chroma samples of each 2x2 block into the single NV12 chroma sample (box
/// filter; the inverse of the OUT-4 NV12→UYVY vertical replication). The line
/// `stride` is honoured so trailing row padding never leaks into a plane. The
/// result is tagged `BT.709 limited` ([`ColorInfo::default`]); the compositor
/// re-tags/converts per tile (invariant #8).
///
/// Pure and panic-free: every buffer access is checked (`.get`), so a malformed
/// frame (already rejected by [`ReceivedVideoFrame::new`]) would yield neutral
/// samples rather than an out-of-bounds read.
///
/// # Errors
/// [`NdiConvertError::UnsupportedFourCc`] if the frame is not UYVY;
/// [`NdiConvertError::InvalidGeometry`] if the geometry overflows `usize`.
pub fn uyvy_to_nv12(frame: &ReceivedVideoFrame) -> Result<HostNv12, NdiConvertError> {
    if frame.fourcc != NdiRecvFourCc::Uyvy {
        return Err(NdiConvertError::UnsupportedFourCc(frame.fourcc));
    }
    let Some((width, height)) = dims_usize(frame) else {
        return Err(NdiConvertError::InvalidGeometry {
            detail: "dimensions overflow usize".to_owned(),
        });
    };
    let stride = usize::try_from(frame.stride).unwrap_or(usize::MAX);
    let src = frame.data();

    let y_len = width.saturating_mul(height);
    let uv_len = y_len / 2;
    let mut y_plane = vec![0u8; y_len];
    let mut uv_plane = vec![0u8; uv_len];

    // Y: verbatim luma, packed tightly into the plane.
    for row in 0..height {
        let row_base = row.saturating_mul(stride);
        let y_row_base = row.saturating_mul(width);
        let mut col = 0usize;
        while col < width {
            // Each UYVY group covers two horizontal pixels: U Y0 V Y1.
            let group = col / 2;
            let group_base = row_base.saturating_add(group.saturating_mul(4));
            let y0 = src.get(group_base.saturating_add(1)).copied().unwrap_or(16);
            let y1 = src.get(group_base.saturating_add(3)).copied().unwrap_or(16);
            if let Some(slot) = y_plane.get_mut(y_row_base.saturating_add(col)) {
                *slot = y0;
            }
            if let Some(slot) = y_plane.get_mut(y_row_base.saturating_add(col).saturating_add(1)) {
                *slot = y1;
            }
            col = col.saturating_add(2);
        }
    }

    // Chroma: average the two vertically-stacked 4:2:2 chroma rows of each 2x2
    // block into the single NV12 chroma row (4:2:2 -> 4:2:0).
    let chroma_pairs_per_row = width / 2;
    let mut chroma_row = 0usize;
    while chroma_row < height / 2 {
        let top = chroma_row.saturating_mul(2);
        let bottom = top.saturating_add(1);
        let top_base = top.saturating_mul(stride);
        let bottom_base = bottom.saturating_mul(stride);
        let uv_row_base = chroma_row
            .saturating_mul(chroma_pairs_per_row)
            .saturating_mul(2);
        for pair in 0..chroma_pairs_per_row {
            let group_off = pair.saturating_mul(4);
            // U is byte 0 of the group, V is byte 2.
            let u_top = src.get(top_base.saturating_add(group_off)).copied();
            let u_bot = src.get(bottom_base.saturating_add(group_off)).copied();
            let v_top = src
                .get(top_base.saturating_add(group_off).saturating_add(2))
                .copied();
            let v_bot = src
                .get(bottom_base.saturating_add(group_off).saturating_add(2))
                .copied();
            let u = average(u_top.unwrap_or(128), u_bot.unwrap_or(128));
            let v = average(v_top.unwrap_or(128), v_bot.unwrap_or(128));
            let out_base = uv_row_base.saturating_add(pair.saturating_mul(2));
            if let Some(slot) = uv_plane.get_mut(out_base) {
                *slot = u;
            }
            if let Some(slot) = uv_plane.get_mut(out_base.saturating_add(1)) {
                *slot = v;
            }
        }
        chroma_row = chroma_row.saturating_add(1);
    }

    Ok(HostNv12 {
        width: frame.width,
        height: frame.height,
        y_plane,
        uv_plane,
        color: ColorInfo::default(),
    })
}

/// Convert a BGRA (8-bit, B G R A byte order) received frame into a [`HostNv12`]
/// using the **BT.709 limited-range** R'G'B' → `Y'CbCr` integer matrix (the canvas's
/// tagged matrix).
///
/// 4:4:4 → 4:2:0: each 2x2 block's four RGB samples are averaged before the matrix
/// (box filter) for the single shared Cb,Cr pair; luma is computed per pixel. The
/// result lands in the BT.709 limited space the tile is tagged with; the
/// compositor's full per-tile colour pipeline (invariant #8) runs against that
/// tag. The alpha channel is dropped (NDI keying/composite is a later overlay
/// concern; the program canvas is opaque NV12).
///
/// Pure and panic-free: all access is checked.
///
/// # Errors
/// [`NdiConvertError::UnsupportedFourCc`] if the frame is not BGRA;
/// [`NdiConvertError::InvalidGeometry`] if the geometry overflows `usize`.
pub fn bgra_to_nv12(frame: &ReceivedVideoFrame) -> Result<HostNv12, NdiConvertError> {
    if frame.fourcc != NdiRecvFourCc::Bgra {
        return Err(NdiConvertError::UnsupportedFourCc(frame.fourcc));
    }
    let Some((width, height)) = dims_usize(frame) else {
        return Err(NdiConvertError::InvalidGeometry {
            detail: "dimensions overflow usize".to_owned(),
        });
    };
    let stride = usize::try_from(frame.stride).unwrap_or(usize::MAX);
    let src = frame.data();

    let y_len = width.saturating_mul(height);
    let uv_len = y_len / 2;
    let mut y_plane = vec![0u8; y_len];
    let mut uv_plane = vec![0u8; uv_len];

    // Per-pixel luma.
    for row in 0..height {
        let row_base = row.saturating_mul(stride);
        let y_row_base = row.saturating_mul(width);
        for col in 0..width {
            let px = row_base.saturating_add(col.saturating_mul(4));
            let (b, g, r) = bgr_at(src, px);
            let y = bt709_luma(r, g, b);
            if let Some(slot) = y_plane.get_mut(y_row_base.saturating_add(col)) {
                *slot = y;
            }
        }
    }

    // Chroma at 4:2:0: average the 2x2 RGB block, then matrix.
    let chroma_pairs_per_row = width / 2;
    let mut cy = 0usize;
    while cy < height / 2 {
        let r0 = cy.saturating_mul(2);
        let r1 = r0.saturating_add(1);
        let uv_row_base = cy.saturating_mul(chroma_pairs_per_row).saturating_mul(2);
        for cx in 0..chroma_pairs_per_row {
            let c0 = cx.saturating_mul(2);
            let c1 = c0.saturating_add(1);
            let mut sb = 0u32;
            let mut sg = 0u32;
            let mut sr = 0u32;
            for (row, col) in [(r0, c0), (r0, c1), (r1, c0), (r1, c1)] {
                let px = row
                    .saturating_mul(stride)
                    .saturating_add(col.saturating_mul(4));
                let (b, g, r) = bgr_at(src, px);
                sb = sb.saturating_add(u32::from(b));
                sg = sg.saturating_add(u32::from(g));
                sr = sr.saturating_add(u32::from(r));
            }
            let avg = |s: u32| u8::try_from(s / 4).unwrap_or(u8::MAX);
            let (cb, cr) = bt709_chroma(avg(sr), avg(sg), avg(sb));
            let out_base = uv_row_base.saturating_add(cx.saturating_mul(2));
            if let Some(slot) = uv_plane.get_mut(out_base) {
                *slot = cb;
            }
            if let Some(slot) = uv_plane.get_mut(out_base.saturating_add(1)) {
                *slot = cr;
            }
        }
        cy = cy.saturating_add(1);
    }

    Ok(HostNv12 {
        width: frame.width,
        height: frame.height,
        y_plane,
        uv_plane,
        color: ColorInfo::default(),
    })
}

/// Read a BGRA pixel's `(b, g, r)` channels at byte offset `px` (checked; a
/// truncated buffer yields neutral 0 channels rather than an out-of-bounds read).
fn bgr_at(src: &[u8], px: usize) -> (u8, u8, u8) {
    let b = src.get(px).copied().unwrap_or(0);
    let g = src.get(px.saturating_add(1)).copied().unwrap_or(0);
    let r = src.get(px.saturating_add(2)).copied().unwrap_or(0);
    (b, g, r)
}

/// Box-filter average of two 8-bit samples, rounding to nearest.
fn average(a: u8, b: u8) -> u8 {
    let sum = u16::from(a).saturating_add(u16::from(b)).saturating_add(1);
    u8::try_from(sum / 2).unwrap_or(u8::MAX)
}

/// BT.709 limited-range luma `Y'` from 8-bit R'G'B' code values.
///
/// `Y' = 16 + (0.2126 R' + 0.7152 G' + 0.0722 B') * 219/255`, evaluated in fixed
/// point (coefficients scaled by 2^16) to stay float-free and deterministic, then
/// clamped to the `[16, 235]` limited-range luma window.
fn bt709_luma(r: u8, g: u8, b: u8) -> u8 {
    // 0.2126, 0.7152, 0.0722 scaled by 65536, pre-multiplied by 219/255.
    const KR: i64 = 11_966; // round(0.2126 * 219/255 * 65536)
    const KG: i64 = 40_254; // round(0.7152 * 219/255 * 65536)
    const KB: i64 = 4_063; // round(0.0722 * 219/255 * 65536)
    let acc = KR
        .saturating_mul(i64::from(r))
        .saturating_add(KG.saturating_mul(i64::from(g)))
        .saturating_add(KB.saturating_mul(i64::from(b)));
    // >> 16 to undo the fixed-point scale, + 16 limited-range offset, rounded.
    let y = 16 + ((acc + 32_768) >> 16);
    clamp_u8(y, 16, 235)
}

/// BT.709 limited-range chroma `(Cb, Cr)` from 8-bit R'G'B' code values.
///
/// Standard BT.709 chroma derivation in fixed point, offset to the 128 neutral
/// point and clamped to the `[16, 240]` limited-range chroma window.
fn bt709_chroma(r: u8, g: u8, b: u8) -> (u8, u8) {
    // Cb = (B' - Y'_full) * 0.5389, Cr = (R' - Y'_full) * 0.6350, each scaled to
    // the 224-wide limited chroma range. We compute against full-range luma so the
    // chroma differences are correct, then scale.
    // Coefficients scaled by 65536 and pre-multiplied by 224/255.
    // Cb: -0.1146 R' - 0.3854 G' + 0.5000 B' (BT.709), * 224/255.
    // Cr:  0.5000 R' - 0.4542 G' - 0.0458 B' (BT.709), * 224/255.
    const CB_R: i64 = -6_596; // round(-0.1146 * 224/255 * 65536)
    const CB_G: i64 = -22_188;
    const CB_B: i64 = 28_784; // round(0.5 * 224/255 * 65536)
    const CR_R: i64 = 28_784;
    const CR_G: i64 = -26_145;
    const CR_B: i64 = -2_639;
    let cb = CB_R
        .saturating_mul(i64::from(r))
        .saturating_add(CB_G.saturating_mul(i64::from(g)))
        .saturating_add(CB_B.saturating_mul(i64::from(b)));
    let cr = CR_R
        .saturating_mul(i64::from(r))
        .saturating_add(CR_G.saturating_mul(i64::from(g)))
        .saturating_add(CR_B.saturating_mul(i64::from(b)));
    let cb = 128 + ((cb + 32_768) >> 16);
    let cr = 128 + ((cr + 32_768) >> 16);
    (clamp_u8(cb, 16, 240), clamp_u8(cr, 16, 240))
}

/// Clamp an `i64` into `[lo, hi]` and narrow to `u8` (both bounds are within
/// `u8`), float-free and panic-free.
fn clamp_u8(v: i64, lo: i64, hi: i64) -> u8 {
    let c = v.clamp(lo, hi);
    u8::try_from(c).unwrap_or(0)
}
