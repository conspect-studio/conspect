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
pub struct AlphaSource {
    /// The decoder's own output frame (pre-NV12), at the decoded size.
    pub frame: Video,
    /// Where its alpha samples live.
    pub layout: AlphaLayout,
}

impl AlphaSource {
    /// Copy the straight alpha into `dst` as one byte per pixel, rows
    /// `dst_stride` bytes apart, rescaling deeper samples to 8 bits.
    ///
    /// # Errors
    /// [`AlphaError`] when `dst` is too short for the frame or the frame's
    /// alpha plane is missing.
    pub fn copy_to(&self, dst: &mut [u8], dst_stride: usize) -> Result<(), AlphaError> {
        if self.layout.plane >= self.frame.planes() {
            return Err(AlphaError::Geometry("frame has no alpha plane"));
        }
        let width = usize::try_from(self.frame.width())
            .map_err(|_| AlphaError::Geometry("width overflows"))?;
        let height = usize::try_from(self.frame.height())
            .map_err(|_| AlphaError::Geometry("height overflows"))?;
        extract_alpha(
            self.layout,
            self.frame.data(self.layout.plane),
            self.frame.stride(self.layout.plane),
            width,
            height,
            dst,
            dst_stride,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

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
