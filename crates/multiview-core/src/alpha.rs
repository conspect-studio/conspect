//! Straight-alpha extraction for alpha media (ADR-0058 NV12+A, #391).
//!
//! A media player whose asset carries an alpha channel publishes NV12 plus one
//! full-resolution 8-bit **straight** alpha plane. Decoders hand that alpha
//! back in many shapes: a separate plane (`yuva420p`, `yuva444p10le`, `gbrap`),
//! or one channel of a packed pixel (`argb` from `qtrle`, `rgba` from PNG,
//! `bgra` from TGA, `rgba64be` from 16-bit PNG). This module is the pure,
//! dependency-free description of where the alpha sample lives
//! ([`AlphaLayout`]) and the one routine that copies it into a tightly packed
//! or stride-padded 8-bit plane ([`extract_alpha`]).
//!
//! It is deliberately colour-free: alpha is never range-expanded, matrixed or
//! linearized (it is coverage, not light). Deeper samples are rescaled to
//! 8 bits with round-to-nearest, `(v * 255 + max / 2) / max`, so the full-scale
//! code (`max`) maps to `255` and `0` maps to `0` exactly.
//!
//! Alpha stays **straight** here; the compositor premultiplies exactly once, in
//! linear light (ADR-0058 §2).

use thiserror::Error;

/// How one alpha sample is stored in its source plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaDepth {
    /// One byte per sample, full scale `255`.
    U8,
    /// A 16-bit little-endian container holding `bits` significant bits
    /// (`9..=16`), full scale `2^bits - 1`.
    U16Le {
        /// Significant bits in the container (9 to 16).
        bits: u8,
    },
    /// A 16-bit big-endian container holding `bits` significant bits
    /// (`9..=16`), full scale `2^bits - 1`.
    U16Be {
        /// Significant bits in the container (9 to 16).
        bits: u8,
    },
}

impl AlphaDepth {
    /// Bytes one sample occupies.
    #[must_use]
    pub const fn sample_bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16Le { .. } | Self::U16Be { .. } => 2,
        }
    }
}

/// Where the alpha sample of every pixel lives in a decoded frame.
///
/// `plane` is the frame's plane index; within each row, pixel `x`'s alpha
/// sample starts at byte `x * step + offset`. A planar alpha plane has
/// `step == sample_bytes` and `offset == 0`; a packed `argb` pixel has
/// `step == 4` and `offset == 0`; `rgba` has `offset == 3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlphaLayout {
    /// Plane index holding the alpha samples.
    pub plane: usize,
    /// Bytes between consecutive pixels' alpha samples in one row.
    pub step: usize,
    /// Byte offset of the alpha sample inside one pixel's `step`.
    pub offset: usize,
    /// Sample storage.
    pub depth: AlphaDepth,
}

impl AlphaLayout {
    /// A dedicated alpha plane (`yuva*`, `gbrap*`).
    #[must_use]
    pub const fn planar(plane: usize, depth: AlphaDepth) -> Self {
        Self {
            plane,
            step: depth.sample_bytes(),
            offset: 0,
            depth,
        }
    }

    /// Alpha packed inside each pixel of plane 0 (`argb`, `rgba`, `ya8`, …).
    #[must_use]
    pub const fn packed(step: usize, offset: usize, depth: AlphaDepth) -> Self {
        Self {
            plane: 0,
            step,
            offset,
            depth,
        }
    }

    /// Bytes one source row of `width` pixels must hold.
    fn row_bytes(self, width: usize) -> Option<usize> {
        if width == 0 {
            return Some(0);
        }
        (width - 1)
            .checked_mul(self.step)?
            .checked_add(self.offset)?
            .checked_add(self.depth.sample_bytes())
    }
}

/// A failed [`extract_alpha`]: the buffers do not match the declared geometry.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum AlphaError {
    /// A source or destination buffer is shorter than the geometry needs, or a
    /// stride is narrower than one row.
    #[error("alpha geometry mismatch: {0}")]
    Geometry(&'static str),
    /// The layout itself is invalid (zero step, or a bit depth outside 9..=16).
    #[error("invalid alpha layout: {0}")]
    Layout(&'static str),
}

/// Copy the straight alpha of a `width x height` region into an 8-bit plane.
///
/// `src` is the source plane (rows `src_stride` bytes apart, laid out per
/// `layout`); `dst` receives one byte per pixel, rows `dst_stride` bytes apart
/// (`dst_stride == width` for a tight plane). Bytes of `dst` beyond each row's
/// `width` are left untouched. Odd widths and heights are fine.
///
/// # Errors
/// [`AlphaError::Geometry`] when a buffer is too short or a stride too narrow;
/// [`AlphaError::Layout`] for a zero step or an unsupported bit depth. Never
/// panics.
pub fn extract_alpha(
    layout: AlphaLayout,
    src: &[u8],
    src_stride: usize,
    width: usize,
    height: usize,
    dst: &mut [u8],
    dst_stride: usize,
) -> Result<(), AlphaError> {
    let sample_end = layout
        .offset
        .checked_add(layout.depth.sample_bytes())
        .ok_or(AlphaError::Layout("step must cover the alpha sample"))?;
    if layout.step == 0 || layout.step < sample_end {
        return Err(AlphaError::Layout("step must cover the alpha sample"));
    }
    let max = match layout.depth {
        AlphaDepth::U8 => 255_u32,
        AlphaDepth::U16Le { bits } | AlphaDepth::U16Be { bits } => {
            if !(9..=16).contains(&bits) {
                return Err(AlphaError::Layout("16-bit alpha needs 9..=16 bits"));
            }
            (1_u32 << bits) - 1
        }
    };
    let src_row = layout
        .row_bytes(width)
        .ok_or(AlphaError::Geometry("row size overflows"))?;
    if src_stride < src_row || dst_stride < width {
        return Err(AlphaError::Geometry("stride narrower than one row"));
    }
    for row in 0..height {
        let s_start = row
            .checked_mul(src_stride)
            .ok_or(AlphaError::Geometry("source offset overflows"))?;
        let d_start = row
            .checked_mul(dst_stride)
            .ok_or(AlphaError::Geometry("destination offset overflows"))?;
        let s_end = s_start
            .checked_add(src_row)
            .ok_or(AlphaError::Geometry("source offset overflows"))?;
        let d_end = d_start
            .checked_add(width)
            .ok_or(AlphaError::Geometry("destination offset overflows"))?;
        let s = src
            .get(s_start..s_end)
            .ok_or(AlphaError::Geometry("source plane too short"))?;
        let d = dst
            .get_mut(d_start..d_end)
            .ok_or(AlphaError::Geometry("destination plane too short"))?;
        let samples = s.get(layout.offset..).unwrap_or_default();
        match layout.depth {
            AlphaDepth::U8 => {
                for (out, a) in d.iter_mut().zip(samples.iter().step_by(layout.step)) {
                    *out = *a;
                }
            }
            AlphaDepth::U16Le { .. } | AlphaDepth::U16Be { .. } => {
                let le = matches!(layout.depth, AlphaDepth::U16Le { .. });
                for (out, pair) in d.iter_mut().zip(
                    samples
                        .chunks(layout.step)
                        .filter_map(|c| c.get(..2).and_then(|p| <[u8; 2]>::try_from(p).ok())),
                ) {
                    let v = if le {
                        u16::from_le_bytes(pair)
                    } else {
                        u16::from_be_bytes(pair)
                    };
                    *out = rescale_to_u8(u32::from(v), max);
                }
            }
        }
    }
    Ok(())
}

/// Round-to-nearest rescale of `v` in `0..=max` to `0..=255` (`v > max` clamps).
fn rescale_to_u8(v: u32, max: u32) -> u8 {
    let v = v.min(max);
    u8::try_from((v * 255 + max / 2) / max).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn planar_u8_odd_size_with_padded_stride_copies_active_samples() {
        // 3x3 alpha, stride 5 (2 bytes of 0xEE padding per row).
        let src = [
            0, 1, 2, 0xEE, 0xEE, //
            10, 11, 12, 0xEE, 0xEE, //
            20, 21, 22, 0xEE, 0xEE,
        ];
        let mut dst = [0_u8; 9];
        extract_alpha(
            AlphaLayout::planar(3, AlphaDepth::U8),
            &src,
            5,
            3,
            3,
            &mut dst,
            3,
        )
        .unwrap();
        assert_eq!(dst, [0, 1, 2, 10, 11, 12, 20, 21, 22]);
    }

    #[test]
    fn planar_10_bit_le_rescales_with_round_to_nearest() {
        let samples: [u16; 5] = [0, 1023, 512, 2, 4095];
        let src: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut dst = [0_u8; 5];
        extract_alpha(
            AlphaLayout::planar(3, AlphaDepth::U16Le { bits: 10 }),
            &src,
            10,
            5,
            1,
            &mut dst,
            5,
        )
        .unwrap();
        // 512*255/1023 = 127.62 -> 128; 2*255/1023 = 0.498 -> 0;
        // out-of-range 4095 clamps to full scale.
        assert_eq!(dst, [0, 255, 128, 0, 255]);
    }

    #[test]
    fn packed_rgba64_be_uses_the_last_16_bit_channel() {
        // Two RGBA64BE pixels: alpha 0xFFFF and 0x8000.
        let src = [
            1, 2, 3, 4, 5, 6, 0xFF, 0xFF, //
            9, 9, 9, 9, 9, 9, 0x80, 0x00,
        ];
        let mut dst = [0_u8; 2];
        extract_alpha(
            AlphaLayout::packed(8, 6, AlphaDepth::U16Be { bits: 16 }),
            &src,
            16,
            2,
            1,
            &mut dst,
            2,
        )
        .unwrap();
        assert_eq!(dst, [255, 128]);
    }

    #[test]
    fn packed_rgba_and_argb_pick_the_right_byte() {
        let rgba = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let mut dst = [0_u8; 3];
        extract_alpha(
            AlphaLayout::packed(4, 3, AlphaDepth::U8),
            &rgba,
            12,
            3,
            1,
            &mut dst,
            3,
        )
        .unwrap();
        assert_eq!(dst, [40, 80, 120]);
        extract_alpha(
            AlphaLayout::packed(4, 0, AlphaDepth::U8),
            &rgba,
            12,
            3,
            1,
            &mut dst,
            3,
        )
        .unwrap();
        assert_eq!(dst, [10, 50, 90]);
    }

    #[test]
    fn destination_padding_is_left_untouched() {
        let src = [7_u8, 8, 9, 10];
        let mut dst = [0xAA_u8; 6];
        extract_alpha(
            AlphaLayout::planar(0, AlphaDepth::U8),
            &src,
            2,
            2,
            2,
            &mut dst,
            3,
        )
        .unwrap();
        assert_eq!(dst, [7, 8, 0xAA, 9, 10, 0xAA]);
    }

    #[test]
    fn short_buffers_and_bad_layouts_are_errors_not_panics() {
        let layout = AlphaLayout::packed(4, 3, AlphaDepth::U8);
        let mut dst = [0_u8; 4];
        assert!(extract_alpha(layout, &[0; 7], 8, 2, 2, &mut dst, 2).is_err());
        assert!(extract_alpha(layout, &[0; 16], 8, 2, 2, &mut [0; 3], 2).is_err());
        assert!(extract_alpha(layout, &[0; 16], 7, 2, 2, &mut dst, 2).is_err());
        let bad = AlphaLayout::packed(2, 3, AlphaDepth::U8);
        assert!(extract_alpha(bad, &[0; 16], 8, 2, 2, &mut dst, 2).is_err());
        let bad_bits = AlphaLayout::planar(3, AlphaDepth::U16Le { bits: 8 });
        assert!(extract_alpha(bad_bits, &[0; 16], 8, 2, 2, &mut dst, 2).is_err());
        // Zero-sized regions are a no-op.
        assert!(extract_alpha(layout, &[], 0, 0, 0, &mut [], 0).is_ok());
    }

    /// PB-3: overflowing layout and row-range arithmetic is an error, never a
    /// panic ("never panics" contract), at every boundary.
    #[test]
    fn overflowing_offsets_strides_and_dimensions_are_errors_not_panics() {
        let mut dst = [0_u8; 8];
        // offset + sample_bytes overflows.
        let huge_offset = AlphaLayout::packed(4, usize::MAX, AlphaDepth::U8);
        assert_eq!(
            extract_alpha(huge_offset, &[0; 8], 4, 2, 1, &mut dst, 2),
            Err(AlphaError::Layout("step must cover the alpha sample"))
        );
        let huge_offset_16 = AlphaLayout::packed(8, usize::MAX - 1, AlphaDepth::U16Le { bits: 10 });
        assert!(matches!(
            extract_alpha(huge_offset_16, &[0; 16], 8, 2, 1, &mut dst, 2),
            Err(AlphaError::Layout(_))
        ));
        let planar = AlphaLayout::planar(0, AlphaDepth::U8);
        // Row 1's source range end (`row * stride + row_bytes`) overflows.
        let src = [0_u8; 8];
        assert!(matches!(
            extract_alpha(planar, &src, usize::MAX - 1, 2, 2, &mut dst, 2),
            Err(AlphaError::Geometry(_))
        ));
        // Row 1's destination range end overflows.
        assert!(matches!(
            extract_alpha(planar, &src, 2, 2, 2, &mut dst, usize::MAX - 1),
            Err(AlphaError::Geometry(_))
        ));
        // Huge strides that do not overflow simply run off the buffers.
        assert!(extract_alpha(planar, &src, usize::MAX / 2, 2, 3, &mut dst, 2).is_err());
        // Huge dimensions.
        assert!(extract_alpha(planar, &src, 2, usize::MAX, 1, &mut dst, usize::MAX).is_err());
        assert!(extract_alpha(planar, &src, 2, 2, usize::MAX, &mut dst, 2).is_err());
        let packed = AlphaLayout::packed(usize::MAX, 0, AlphaDepth::U8);
        assert!(extract_alpha(packed, &src, usize::MAX, 3, 1, &mut dst, 3).is_err());
    }
}
