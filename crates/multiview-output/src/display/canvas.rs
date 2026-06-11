//! The display sink's view of a composited frame, and the v1 software
//! NV12→XRGB conversion.
//!
//! The sink stays decoupled from the compositor crate by consuming frames
//! through the object-safe [`DisplayCanvas`] trait (tightly-packed NV12 — the
//! workspace-canonical pixel format, invariant #5). The CLI adapts its canvas
//! type onto this trait.
//!
//! [`nv12_to_xrgb`] is the **CPU scanout conversion** (DEV-B1): BT.709
//! limited-range YCbCr → full-range RGB in 8.8 fixed-point integer math (no
//! floats anywhere), written straight into a stride-aware XRGB8888 scanout
//! mapping, centred with black borders when the canvas and mode geometry
//! differ. DEV-B3 adds the per-hardware *faster* paths the
//! [`super::strategy`] selector chooses when their preconditions hold (NV12
//! direct scanout on Intel/vc4 via a dmabuf canvas; the wgpu pass elsewhere
//! when wired) — but this CPU conversion **remains the guaranteed default and
//! fallback**: it needs only a CPU NV12 canvas + an XRGB plane, takes the
//! frame whenever the canvas is CPU-resident or a faster path is unavailable,
//! and is golden-tested in CI. The [`DisplayCanvas::delivery`] /
//! [`DisplayCanvas::dmabuf_image`] accessors are how a dmabuf-backed canvas
//! opts into those faster paths.

use std::os::fd::BorrowedFd;

use thiserror::Error;

use super::strategy::{CanvasDelivery, DrmFormat};

/// One plane of an importable dmabuf-backed canvas: the borrowed prime fd plus
/// where that plane starts and how wide its rows are. Borrowed for the
/// duration of one scan-out submission — the canvas owns the fd, the backend
/// imports it (`prime_fd_to_buffer`) and adds a framebuffer over it.
#[derive(Debug, Clone, Copy)]
pub struct DmabufPlane<'a> {
    /// The prime (dmabuf) file descriptor backing this plane (planes may share
    /// one fd at different offsets, as NV12 typically does).
    pub fd: BorrowedFd<'a>,
    /// Byte offset of this plane within the dmabuf.
    pub offset: u32,
    /// Row pitch (stride) in bytes.
    pub pitch: u32,
}

/// A dmabuf-backed canvas image ready for **NV12-direct** scanout: the format,
/// modifier, geometry, and per-plane fds/layout the backend needs to
/// `ADDFB2`-import it and flip it with **0 copies, 0 render passes**
/// (display-out.md §2). Only meaningful when the format is plane-scannable
/// (NV12/P010) and the modifier matches the plane (the
/// [`super::strategy::select_buffer_strategy`] gate).
#[derive(Debug, Clone)]
pub struct DmabufImage<'a> {
    /// The dmabuf pixel format (NV12/P010 for direct scanout).
    pub format: DrmFormat,
    /// The dmabuf modifier (`None` == linear); SAND128 for vc4 decoder buffers.
    pub modifier: Option<u64>,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// The planes (1–4), in fourcc plane order (NV12 = [Y, UV]).
    pub planes: Vec<DmabufPlane<'a>>,
}

/// An NV12 frame the display sink can scan out: tightly-packed planes
/// (`y.len() == width*height`, `uv.len() == width*height/2`), even
/// dimensions (4:2:0 chroma).
pub trait DisplayCanvas {
    /// Frame width in pixels (even).
    fn width(&self) -> u32;
    /// Frame height in pixels (even).
    fn height(&self) -> u32;
    /// The luma plane, `width * height` bytes, no row padding.
    fn y_plane(&self) -> &[u8];
    /// The interleaved Cb/Cr plane, `width * height / 2` bytes, no padding.
    fn uv_plane(&self) -> &[u8];

    /// How this canvas is delivered — CPU-resident planes (the default,
    /// DEV-B1) or an importable dmabuf. The buffer-strategy selector
    /// ([`super::strategy::select_buffer_strategy`]) reads this to decide
    /// NV12-direct / wgpu-pass scanout vs the portable CPU conversion: a
    /// CPU-planes canvas always takes the CPU convert (there is no dmabuf to
    /// import or flip).
    ///
    /// The default is [`CanvasDelivery::CpuPlanes`]; a canvas backed by a
    /// decoder/compositor dmabuf overrides it. The `y_plane`/`uv_plane`
    /// accessors must remain valid regardless (they are the universal CPU
    /// fallback path even for a dmabuf-backed canvas).
    fn delivery(&self) -> CanvasDelivery {
        CanvasDelivery::CpuPlanes
    }

    /// The borrowed dmabuf fds + plane layout for **NV12-direct** scanout, or
    /// [`None`] for a CPU-planes canvas. Consistent with [`Self::delivery`]: a
    /// canvas that reports [`CanvasDelivery::Dmabuf`] returns `Some` here, and
    /// the NV12-direct backend path (`ADDFB2`-import + flip) uses it. The fds
    /// are borrowed for the call only — the backend imports a GEM handle from
    /// them synchronously and never retains the borrow.
    ///
    /// The default is [`None`] (the CPU-planes path); only a dmabuf-backed
    /// canvas overrides it.
    fn dmabuf_image(&self) -> Option<DmabufImage<'_>> {
        None
    }
}

/// A structurally invalid conversion request (geometry/plane mismatch).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CanvasError {
    /// The destination buffer/stride cannot hold the requested geometry.
    #[error("destination geometry invalid: {0}")]
    Destination(String),
    /// The source planes do not match the frame's declared geometry.
    #[error("source NV12 planes invalid: {0}")]
    Source(String),
}

/// Clamp a fixed-point intermediate to an 8-bit channel.
fn clamp_u8(v: i32) -> u8 {
    // The clamp bounds the value into 0..=255, so the conversion never fails;
    // `unwrap_or` is unreachable and exists only to avoid a panicking path.
    u8::try_from(v.clamp(0, 255)).unwrap_or(0)
}

/// One BT.709 limited-range YCbCr sample → full-range `[r, g, b]`.
///
/// 8.8 fixed point, the standard integer coefficients:
/// `R = 1.164·(Y−16) + 1.793·(Cr−128)`, `G = 1.164·(Y−16) − 0.213·(Cb−128) −
/// 0.533·(Cr−128)`, `B = 1.164·(Y−16) + 2.112·(Cb−128)`.
fn bt709_limited_px(luma: u8, cb: u8, cr: u8) -> [u8; 3] {
    let lifted = i32::from(luma) - 16;
    let blue_diff = i32::from(cb) - 128;
    let red_diff = i32::from(cr) - 128;
    let red = clamp_u8((298 * lifted + 459 * red_diff + 128) >> 8);
    let green = clamp_u8((298 * lifted - 55 * blue_diff - 136 * red_diff + 128) >> 8);
    let blue = clamp_u8((298 * lifted + 541 * blue_diff + 128) >> 8);
    [red, green, blue]
}

/// Convert `frame` (NV12, BT.709 limited range — the workspace's SDR default)
/// into a little-endian **XRGB8888** scanout buffer of `dst_width x
/// dst_height` pixels with `dst_stride` bytes per row, centring the frame and
/// filling any border black.
///
/// Pure CPU integer math; allocation-free; never panics. The caller maps the
/// scanout buffer (GBM BO or dumb buffer) and hands the mapping in.
///
/// # Errors
///
/// Returns [`CanvasError`] when the destination stride/length cannot hold the
/// requested geometry or the source planes are shorter than the frame's
/// declared geometry. Nothing is partially written on error.
pub fn nv12_to_xrgb(
    frame: &dyn DisplayCanvas,
    dst: &mut [u8],
    dst_width: u32,
    dst_height: u32,
    dst_stride: u32,
) -> Result<(), CanvasError> {
    const BYTES_PER_PX: usize = 4;
    let dst_w = usize::try_from(dst_width)
        .map_err(|_| CanvasError::Destination("width overflows usize".to_owned()))?;
    let dst_h = usize::try_from(dst_height)
        .map_err(|_| CanvasError::Destination("height overflows usize".to_owned()))?;
    let stride = usize::try_from(dst_stride)
        .map_err(|_| CanvasError::Destination("stride overflows usize".to_owned()))?;
    if stride < dst_w.saturating_mul(BYTES_PER_PX) {
        return Err(CanvasError::Destination(format!(
            "stride {stride} < {dst_w} px * 4 B"
        )));
    }
    let needed = stride.saturating_mul(dst_h);
    if dst.len() < needed {
        return Err(CanvasError::Destination(format!(
            "buffer {} B < required {needed} B",
            dst.len()
        )));
    }

    let src_w = usize::try_from(frame.width())
        .map_err(|_| CanvasError::Source("width overflows usize".to_owned()))?;
    let src_h = usize::try_from(frame.height())
        .map_err(|_| CanvasError::Source("height overflows usize".to_owned()))?;
    let y_plane = frame.y_plane();
    let uv_plane = frame.uv_plane();
    if y_plane.len() < src_w.saturating_mul(src_h) {
        return Err(CanvasError::Source(format!(
            "Y plane {} B < {src_w}x{src_h}",
            y_plane.len()
        )));
    }
    if uv_plane.len() < src_w.saturating_mul(src_h) / 2 {
        return Err(CanvasError::Source(format!(
            "UV plane {} B < {src_w}x{src_h}/2",
            uv_plane.len()
        )));
    }

    // Copy geometry: the centred intersection of frame and mode.
    let copy_w = src_w.min(dst_w) & !1;
    let copy_h = src_h.min(dst_h) & !1;
    let dst_col0 = (dst_w - copy_w) / 2;
    let dst_row0 = (dst_h - copy_h) / 2;
    let src_col0 = (src_w - copy_w) / 2;
    let src_row0 = (src_h - copy_h) / 2;

    for (row_idx, dst_row) in dst.chunks_exact_mut(stride).take(dst_h).enumerate() {
        // Black borders: X=0, R=G=B=0 — zero the full row first, then write
        // the converted span over the centre. (Rows outside the copy band are
        // entirely border.)
        for b in dst_row.iter_mut().take(dst_w * BYTES_PER_PX) {
            *b = 0;
        }
        let Some(src_row) = row_idx
            .checked_sub(dst_row0)
            .filter(|r| *r < copy_h)
            .map(|r| r + src_row0)
        else {
            continue;
        };
        let y_start = src_row * src_w + src_col0;
        let uv_start = (src_row / 2) * src_w + (src_col0 & !1);
        let (Some(y_row), Some(uv_row)) = (
            y_plane.get(y_start..y_start + copy_w),
            uv_plane.get(uv_start..uv_start + copy_w),
        ) else {
            // Geometry was validated above; a short read here means the trait
            // impl lied about its planes — skip the row rather than panic.
            continue;
        };
        let px_start = dst_col0 * BYTES_PER_PX;
        let Some(out_span) = dst_row.get_mut(px_start..px_start + copy_w * BYTES_PER_PX) else {
            continue;
        };
        // Walk two pixels at a time: one Cb/Cr pair covers a 2x1 luma span.
        for ((y_pair, uv_pair), out_pair) in y_row
            .chunks_exact(2)
            .zip(uv_row.chunks_exact(2))
            .zip(out_span.chunks_exact_mut(2 * BYTES_PER_PX))
        {
            let (Some(&cb), Some(&cr)) = (uv_pair.first(), uv_pair.get(1)) else {
                continue;
            };
            for (y_val, out_px) in y_pair.iter().zip(out_pair.chunks_exact_mut(BYTES_PER_PX)) {
                let [r, g, b] = bt709_limited_px(*y_val, cb, cr);
                // XRGB8888 little-endian memory order: B, G, R, X.
                if let [ob, og, or, ox] = out_px {
                    *ob = b;
                    *og = g;
                    *or = r;
                    *ox = 0;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny owned NV12 frame for the conversion goldens.
    struct Frame {
        w: u32,
        h: u32,
        y: Vec<u8>,
        uv: Vec<u8>,
    }

    impl DisplayCanvas for Frame {
        fn width(&self) -> u32 {
            self.w
        }
        fn height(&self) -> u32 {
            self.h
        }
        fn y_plane(&self) -> &[u8] {
            &self.y
        }
        fn uv_plane(&self) -> &[u8] {
            &self.uv
        }
    }

    fn solid(w: u32, h: u32, y: u8, cb: u8, cr: u8) -> Frame {
        let pixels = usize::try_from(w * h).expect("test geometry fits usize");
        Frame {
            w,
            h,
            y: vec![y; pixels],
            uv: {
                let mut uv = Vec::with_capacity(pixels / 2);
                for _ in 0..pixels / 4 {
                    uv.push(cb);
                    uv.push(cr);
                }
                uv
            },
        }
    }

    #[test]
    fn black_and_white_hit_the_full_range_rails() {
        // Limited-range black (16) → 0; limited-range white (235) → 255.
        let mut dst = vec![0xAAu8; 2 * 2 * 4];
        nv12_to_xrgb(&solid(2, 2, 16, 128, 128), &mut dst, 2, 2, 8).expect("converts");
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]);
        nv12_to_xrgb(&solid(2, 2, 235, 128, 128), &mut dst, 2, 2, 8).expect("converts");
        assert_eq!(&dst[0..4], &[255, 255, 255, 0]);
    }

    #[test]
    fn bt709_red_lands_on_red() {
        // BT.709 limited-range pure red ≈ (Y, Cb, Cr) = (63, 102, 240).
        let mut dst = vec![0u8; 2 * 2 * 4];
        nv12_to_xrgb(&solid(2, 2, 63, 102, 240), &mut dst, 2, 2, 8).expect("converts");
        let (b, g, r) = (dst[0], dst[1], dst[2]);
        assert!(r > 240, "red channel ~255, got {r}");
        assert!(g < 16, "green channel ~0, got {g}");
        assert!(b < 16, "blue channel ~0, got {b}");
    }

    #[test]
    fn smaller_canvas_is_centred_with_black_borders() {
        // A 2x2 white frame into a 4x4 mode: the centre 2x2 is white, the
        // border is black.
        let mut dst = vec![0xAAu8; 4 * 4 * 4];
        nv12_to_xrgb(&solid(2, 2, 235, 128, 128), &mut dst, 4, 4, 16).expect("converts");
        // Top-left border pixel: black.
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]);
        // Centre pixel (row 1, col 1 => byte 16 + 4 = 20): white.
        let centre = 20;
        assert_eq!(&dst[centre..centre + 4], &[255, 255, 255, 0]);
    }

    #[test]
    fn short_destination_is_a_typed_error_not_a_panic() {
        let mut dst = vec![0u8; 4];
        let err = nv12_to_xrgb(&solid(2, 2, 16, 128, 128), &mut dst, 2, 2, 8);
        assert!(matches!(err, Err(CanvasError::Destination(_))));
    }

    #[test]
    fn default_canvas_delivery_is_cpu_planes() {
        // A canvas that does not override `delivery` is CPU-resident planes —
        // the DEV-B1 path. The strategy selector reads this to refuse
        // direct/GPU scanout for a CPU-only canvas (no dmabuf to import).
        use crate::display::strategy::CanvasDelivery;
        let frame = solid(2, 2, 16, 128, 128);
        assert_eq!(DisplayCanvas::delivery(&frame), CanvasDelivery::CpuPlanes);
    }

    #[test]
    fn a_canvas_can_advertise_an_importable_nv12_dmabuf() {
        use crate::display::strategy::{CanvasDelivery, DrmFormat, DRM_FORMAT_MOD_LINEAR};

        // A canvas wrapping an imported NV12 dmabuf overrides `delivery`; the
        // selector then becomes eligible for NV12-direct / wgpu-pass scanout.
        struct DmabufFrame(Frame);
        impl DisplayCanvas for DmabufFrame {
            fn width(&self) -> u32 {
                self.0.width()
            }
            fn height(&self) -> u32 {
                self.0.height()
            }
            fn y_plane(&self) -> &[u8] {
                self.0.y_plane()
            }
            fn uv_plane(&self) -> &[u8] {
                self.0.uv_plane()
            }
            fn delivery(&self) -> CanvasDelivery {
                CanvasDelivery::Dmabuf {
                    format: DrmFormat::NV12,
                    modifier: Some(DRM_FORMAT_MOD_LINEAR),
                }
            }
        }
        let frame = DmabufFrame(solid(2, 2, 16, 128, 128));
        assert_eq!(
            frame.delivery(),
            CanvasDelivery::Dmabuf {
                format: DrmFormat::NV12,
                modifier: Some(DRM_FORMAT_MOD_LINEAR),
            }
        );
    }

    #[test]
    fn default_canvas_exposes_no_importable_dmabuf_image() {
        // The CPU-planes default never offers a dmabuf to import; the
        // NV12-direct backend path therefore never fires for it.
        let frame = solid(2, 2, 16, 128, 128);
        assert!(DisplayCanvas::dmabuf_image(&frame).is_none());
    }

    #[test]
    fn a_dmabuf_canvas_exposes_its_borrowed_fds_and_plane_layout() {
        use crate::display::strategy::{DrmFormat, DRM_FORMAT_MOD_LINEAR};
        use std::os::fd::AsFd;

        struct DmabufFrame<'a> {
            inner: Frame,
            fd: std::os::fd::BorrowedFd<'a>,
        }
        impl DisplayCanvas for DmabufFrame<'_> {
            fn width(&self) -> u32 {
                self.inner.width()
            }
            fn height(&self) -> u32 {
                self.inner.height()
            }
            fn y_plane(&self) -> &[u8] {
                self.inner.y_plane()
            }
            fn uv_plane(&self) -> &[u8] {
                self.inner.uv_plane()
            }
            fn dmabuf_image(&self) -> Option<DmabufImage<'_>> {
                Some(DmabufImage {
                    format: DrmFormat::NV12,
                    modifier: Some(DRM_FORMAT_MOD_LINEAR),
                    width: self.inner.width(),
                    height: self.inner.height(),
                    planes: vec![
                        DmabufPlane {
                            fd: self.fd,
                            offset: 0,
                            pitch: self.inner.width(),
                        },
                        DmabufPlane {
                            fd: self.fd,
                            offset: self.inner.width() * self.inner.height(),
                            pitch: self.inner.width(),
                        },
                    ],
                })
            }
        }

        // Use a real fd (stdin) just to have a valid BorrowedFd for the test.
        let stdin = std::io::stdin();
        let frame = DmabufFrame {
            inner: solid(4, 4, 16, 128, 128),
            fd: stdin.as_fd(),
        };
        let image = DisplayCanvas::dmabuf_image(&frame).expect("dmabuf present");
        assert_eq!(image.format, DrmFormat::NV12);
        assert_eq!(image.planes.len(), 2);
        assert_eq!(image.planes[0].offset, 0);
        assert_eq!(image.planes[1].offset, 16);
    }
}
