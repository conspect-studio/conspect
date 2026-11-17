//! Alpha-capable decoded frames (ADR-0058 NV12+A, #391).
//!
//! [`alpha_layout`] maps a libav pixel format to where its straight alpha lives
//! ([`AlphaLayout`]), or `None` for an opaque format. Opaque formats are the
//! overwhelming case and cost exactly one `match` here.
//!
//! [`AlphaSource`] is the decoded frame handed back alongside the NV12
//! conversion when the decoder emitted an alpha format: the libav frame itself
//! (a reference-counted `AVFrame` moved out of the decoder, never a pixel copy)
//! plus its layout. A media player turns it into the full-resolution R8 plane
//! of an NV12+A image; every other consumer simply ignores it.
//!
//! Alpha from these decoders is **straight** (ProRes 4444, `qtrle`, PNG, TGA,
//! `libvpx-vp9` alpha all store unassociated alpha). No libav decoder marks
//! premultiplied output, so nothing here un-premultiplies.

use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video;
use ffmpeg_next as ffmpeg;

use crate::frame::ScratchFrame;

pub use multiview_core::alpha::{extract_alpha, AlphaDepth, AlphaError, AlphaLayout};

/// The alpha layout of `format`, or `None` when the format carries no alpha.
///
/// Covers the formats the alpha media decoders emit: planar `yuva*` (`ProRes`
/// 4444 `yuva444p10le`/`yuva444p12le`, `libvpx-vp9` `yuva420p`), planar
/// `gbrap*`, packed 8-bit `argb`/`rgba`/`abgr`/`bgra` (`qtrle`, PNG, TGA),
/// packed 16-bit `rgba64`/`bgra64` (16-bit PNG) and grey+alpha `ya8`/`ya16`.
/// Paletted `pal8` (PNG/GIF palette alpha) is not mapped: its alpha lives in
/// the palette, which libswscale resolves only on conversion.
#[must_use]
pub fn alpha_layout(format: Pixel) -> Option<AlphaLayout> {
    use AlphaDepth::{U16Be, U16Le, U8};
    let layout = match format {
        // Planar YUV + A and GBR + A: plane 3.
        Pixel::YUVA420P | Pixel::YUVA422P | Pixel::YUVA444P | Pixel::GBRAP => {
            AlphaLayout::planar(3, U8)
        }
        Pixel::YUVA420P9LE | Pixel::YUVA422P9LE | Pixel::YUVA444P9LE => {
            AlphaLayout::planar(3, U16Le { bits: 9 })
        }
        Pixel::YUVA420P9BE | Pixel::YUVA422P9BE | Pixel::YUVA444P9BE => {
            AlphaLayout::planar(3, U16Be { bits: 9 })
        }
        Pixel::YUVA420P10LE | Pixel::YUVA422P10LE | Pixel::YUVA444P10LE | Pixel::GBRAP10LE => {
            AlphaLayout::planar(3, U16Le { bits: 10 })
        }
        Pixel::YUVA420P10BE | Pixel::YUVA422P10BE | Pixel::YUVA444P10BE | Pixel::GBRAP10BE => {
            AlphaLayout::planar(3, U16Be { bits: 10 })
        }
        Pixel::YUVA422P12LE | Pixel::YUVA444P12LE | Pixel::GBRAP12LE => {
            AlphaLayout::planar(3, U16Le { bits: 12 })
        }
        Pixel::YUVA422P12BE | Pixel::YUVA444P12BE | Pixel::GBRAP12BE => {
            AlphaLayout::planar(3, U16Be { bits: 12 })
        }
        Pixel::YUVA420P16LE | Pixel::YUVA422P16LE | Pixel::YUVA444P16LE | Pixel::GBRAP16LE => {
            AlphaLayout::planar(3, U16Le { bits: 16 })
        }
        Pixel::YUVA420P16BE | Pixel::YUVA422P16BE | Pixel::YUVA444P16BE | Pixel::GBRAP16BE => {
            AlphaLayout::planar(3, U16Be { bits: 16 })
        }
        // Packed 8-bit, 4 bytes per pixel (byte order as named).
        Pixel::ARGB | Pixel::ABGR => AlphaLayout::packed(4, 0, U8),
        Pixel::RGBA | Pixel::BGRA => AlphaLayout::packed(4, 3, U8),
        // Packed 16-bit, 8 bytes per pixel, alpha last.
        Pixel::RGBA64LE | Pixel::BGRA64LE => AlphaLayout::packed(8, 6, U16Le { bits: 16 }),
        Pixel::RGBA64BE | Pixel::BGRA64BE => AlphaLayout::packed(8, 6, U16Be { bits: 16 }),
        // Grey + alpha.
        Pixel::YA8 => AlphaLayout::packed(2, 1, U8),
        Pixel::YA16LE => AlphaLayout::packed(4, 2, U16Le { bits: 16 }),
        Pixel::YA16BE => AlphaLayout::packed(4, 2, U16Be { bits: 16 }),
        _ => return None,
    };
    Some(layout)
}

/// A decoded frame whose pixel format carries straight alpha, kept beside its
/// NV12 conversion so a media player can publish NV12+A.
///
/// # Trusted provenance (review PB-14)
/// [`Self::copy_to`] builds Rust row slices over the frame through the
/// crate-private `frame::plane_rows`, whose row width and count come from
/// the frame's own format, width and height. Those must be the ones its
/// bytes were written for: the safe `Video::set_width`/`set_height` could
/// otherwise widen a decoder frame over its unwritten stride padding (or
/// its padded extra rows) inside the same allocation, and the copy would
/// read uninitialised memory. So the fields are private and only two
/// provenances exist:
///
/// * the decoder inside this crate ([`crate::decode_stream`]), which moves
///   its output frame in untouched;
/// * [`Self::from_checked`], from a [`ScratchFrame`] (allocated and
///   zero-initialised by `alloc_video`, its geometry never mutable).
///
/// Afterwards only `&Video` is handed out ([`Self::frame`]), so nothing can
/// change the geometry. An arbitrary `Video` cannot become an
/// `AlphaSource` outside this crate:
///
/// ```compile_fail,E0451
/// use ffmpeg_next::format::Pixel;
/// use ffmpeg_next::util::frame::Video;
/// use multiview_ffmpeg::alpha::{alpha_layout, AlphaSource};
/// let mut frame = Video::new(Pixel::RGBA, 5, 2);
/// frame.set_width(6); // widened over the stride padding
/// let layout = alpha_layout(Pixel::RGBA).unwrap();
/// let source = AlphaSource { frame, layout }; // private fields
/// ```
///
/// ```compile_fail,E0624
/// use ffmpeg_next::format::Pixel;
/// use ffmpeg_next::util::frame::Video;
/// use multiview_ffmpeg::alpha::{alpha_layout, AlphaSource};
/// let frame = Video::new(Pixel::RGBA, 5, 2);
/// let layout = alpha_layout(Pixel::RGBA).unwrap();
/// let source = AlphaSource::from_decoder(frame, layout); // crate-private
/// ```
///
/// ```compile_fail,E0596
/// use ffmpeg_next::format::Pixel;
/// use multiview_ffmpeg::alpha::{alpha_layout, AlphaSource};
/// use multiview_ffmpeg::frame::ScratchFrame;
/// let scratch = ScratchFrame::alloc(Pixel::RGBA, 5, 2).unwrap();
/// let source = AlphaSource::from_checked(scratch, alpha_layout(Pixel::RGBA).unwrap());
/// source.frame().set_width(6); // only a shared reference is handed out
/// ```
pub struct AlphaSource {
    /// The decoder's own output frame (pre-NV12), at the decoded size, or a
    /// checked scratch frame; its geometry is never changed after
    /// construction.
    frame: Video,
    /// Where its alpha samples live.
    layout: AlphaLayout,
}

impl AlphaSource {
    /// The decoder's own output frame, moved in untouched: its format,
    /// width and height are the ones the decoder wrote its rows for, which
    /// [`Self::copy_to`] relies on (see the type docs). Only the decoder in
    /// this crate may call it.
    pub(crate) const fn from_decoder(frame: Video, layout: AlphaLayout) -> Self {
        Self { frame, layout }
    }

    /// An alpha source over a [`ScratchFrame`]: allocated by the checked
    /// `alloc_video` (every owned byte zero-initialised, review PB-12) and
    /// never exposed mutably, so its geometry is still the allocation's, and
    /// written only by `plane_mut` or a `Scaler::run_into` from another
    /// `ScratchFrame` (review PB-15), so every row the copy can view is
    /// initialised (the [`ScratchFrame`] provenance invariant). `layout` is checked
    /// against the frame's format by [`Self::copy_to`].
    #[must_use]
    pub fn from_checked(frame: ScratchFrame, layout: AlphaLayout) -> Self {
        Self {
            frame: frame.into_video(),
            layout,
        }
    }

    /// The frame, read-only (geometry, format, a libav read such as
    /// [`crate::Scaler::run`]).
    #[must_use]
    pub const fn frame(&self) -> &Video {
        &self.frame
    }

    /// Where the frame's alpha samples live.
    #[must_use]
    pub const fn layout(&self) -> AlphaLayout {
        self.layout
    }

    /// Copy the straight alpha into `dst` as one byte per pixel, rows
    /// `dst_stride` bytes apart, rescaling deeper samples to 8 bits.
    ///
    /// The frame's layout is validated before any of its memory is viewed:
    /// `layout` must be the one its pixel format maps to, and the alpha plane
    /// is read through `frame::plane_rows` (signed linesize, plane
    /// height and every row span checked against the frame's own buffers), one
    /// row at a time, so a negative (bottom-up) linesize is copied in display
    /// order rather than becoming a huge slice.
    ///
    /// # Errors
    /// [`AlphaError::Layout`] when `layout` does not describe the frame's
    /// format; [`AlphaError::Geometry`] when `dst` is too short or the frame's
    /// alpha plane is missing or malformed.
    pub fn copy_to(&self, dst: &mut [u8], dst_stride: usize) -> Result<(), AlphaError> {
        if alpha_layout(self.frame.format()) != Some(self.layout) {
            return Err(AlphaError::Layout("layout does not match the frame format"));
        }
        let width = usize::try_from(self.frame.width())
            .map_err(|_| AlphaError::Geometry("width overflows"))?;
        let height = usize::try_from(self.frame.height())
            .map_err(|_| AlphaError::Geometry("height overflows"))?;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let row_bytes = (width - 1)
            .checked_mul(self.layout.step)
            .and_then(|v| v.checked_add(self.layout.offset))
            .and_then(|v| v.checked_add(self.layout.depth.sample_bytes()))
            .ok_or(AlphaError::Geometry("row size overflows"))?;
        let rows = crate::frame::plane_rows(&self.frame, self.layout.plane, row_bytes).map_err(
            |e| match e {
                crate::FfmpegError::FrameLayout(why) => AlphaError::Geometry(why),
                _ => AlphaError::Geometry("alpha plane is unusable"),
            },
        )?;
        let mut copied = 0_usize;
        for (row, src) in rows.enumerate() {
            let start = row
                .checked_mul(dst_stride)
                .ok_or(AlphaError::Geometry("destination offset overflows"))?;
            let end = start
                .checked_add(width)
                .ok_or(AlphaError::Geometry("destination offset overflows"))?;
            let out = dst
                .get_mut(start..end)
                .ok_or(AlphaError::Geometry("destination plane too short"))?;
            extract_alpha(self.layout, src, row_bytes, width, 1, out, width)?;
            copied += 1;
        }
        if copied != height {
            return Err(AlphaError::Geometry("alpha plane height mismatch"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::cast_possible_truncation
    )]
    use super::*;

    /// A `w x h` RGBA frame whose alpha at `(x, y)` is `10 * y + x`.
    fn rgba(w: u32, h: u32) -> Video {
        let mut frame = Video::new(Pixel::RGBA, w, h);
        let stride = frame.stride(0);
        let data = frame.data_mut(0);
        for y in 0..h as usize {
            for x in 0..w as usize {
                data[y * stride + 4 * x..y * stride + 4 * x + 4].copy_from_slice(&[
                    1,
                    2,
                    3,
                    u8::try_from(10 * y + x).unwrap(),
                ]);
            }
        }
        frame
    }

    /// Re-point `frame` plane 0 at its last row with a negated linesize: the
    /// bottom-up layout libav's `vflip` produces (a valid `AVFrame`).
    fn flip_bottom_up(frame: &mut Video) {
        let rows = isize::try_from(frame.height()).unwrap() - 1;
        #[allow(unsafe_code)]
        // SAFETY: the test owns `frame` (refcounted buffer from
        // `av_frame_get_buffer`); the new data pointer is the start of the
        // last row, inside that buffer, and `av_frame_free` unrefs `buf[0]`
        // (not `data[0]`), so dropping the frame stays sound.
        unsafe {
            let f = frame.as_mut_ptr();
            let linesize = (*f).linesize[0];
            (*f).data[0] = (*f).data[0].offset(isize::try_from(linesize).unwrap() * rows);
            (*f).linesize[0] = -linesize;
        }
    }

    /// PB-1: a negative linesize is a valid `AVFrame` layout; the copy walks
    /// it row by row (never a `linesize * height` slice) and gets the rows in
    /// display order.
    #[test]
    fn negative_linesize_alpha_is_copied_in_display_order() {
        let (w, h) = (4_u32, 3_u32);
        let mut frame = rgba(w, h);
        flip_bottom_up(&mut frame);
        let source = AlphaSource {
            frame,
            layout: alpha_layout(Pixel::RGBA).unwrap(),
        };
        let mut dst = [0_u8; 12];
        source.copy_to(&mut dst, 4).unwrap();
        // Row 0 of the flipped frame is the original bottom row.
        assert_eq!(dst, [20, 21, 22, 23, 10, 11, 12, 13, 0, 1, 2, 3]);
    }

    /// PB-13: on a bottom-up plane too, rows wider than the active bytes
    /// (into the stride padding) are refused and the active width walks every
    /// row.
    #[test]
    fn a_bottom_up_plane_never_yields_its_padding() {
        let (w, h) = (5_u32, 3_u32);
        let mut frame = rgba(w, h);
        flip_bottom_up(&mut frame);
        let active = usize::try_from(w).unwrap() * 4;
        assert!(crate::frame::plane_rows(&frame, 0, active + 1).is_err());
        assert_eq!(
            crate::frame::plane_rows(&frame, 0, active).unwrap().count(),
            usize::try_from(h).unwrap()
        );
    }

    /// PB-1: a layout that does not describe the frame's format is refused
    /// before any plane is read.
    #[test]
    fn a_layout_that_does_not_match_the_format_is_an_error() {
        let source = AlphaSource {
            frame: rgba(4, 2),
            layout: AlphaLayout::packed(8, 6, AlphaDepth::U16Le { bits: 16 }),
        };
        let mut dst = [0_u8; 8];
        assert!(matches!(
            source.copy_to(&mut dst, 4),
            Err(AlphaError::Layout(_))
        ));
    }

    #[test]
    fn opaque_formats_have_no_alpha_layout() {
        for format in [
            Pixel::NV12,
            Pixel::P010LE,
            Pixel::YUV420P,
            Pixel::YUV422P10LE,
            Pixel::RGB24,
            Pixel::GBRP,
        ] {
            assert_eq!(alpha_layout(format), None, "{format:?}");
        }
    }

    #[test]
    fn alpha_formats_map_to_the_alpha_plane_or_channel() {
        assert_eq!(
            alpha_layout(Pixel::YUVA444P10LE),
            Some(AlphaLayout::planar(3, AlphaDepth::U16Le { bits: 10 }))
        );
        assert_eq!(
            alpha_layout(Pixel::ARGB),
            Some(AlphaLayout::packed(4, 0, AlphaDepth::U8))
        );
        assert_eq!(
            alpha_layout(Pixel::BGRA),
            Some(AlphaLayout::packed(4, 3, AlphaDepth::U8))
        );
    }
}
