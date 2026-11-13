//! Pure-integer NV12 cell scaler shared by the monitor wall (ADR-0103), the
//! routing aux canvases (ADR-0108 §2.4) and the per-output format convert
//! (#342). Colour samples are copied, never converted: the caller guarantees
//! the source and destination share a tag.

use multiview_compositor::pipeline::Nv12Image;
use multiview_control::preview::WallCell;

use crate::output_fit::Rect;

/// Scale `src` into the rectangle `cell` of the wall planes by 2x2 box sampling
/// at nearest source positions. Pure integer arithmetic, no allocation.
pub fn blit_cell(
    src: &Nv12Image,
    wall_width: u32,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    cell: &WallCell,
) {
    blit_region(
        src,
        Rect::new(0, 0, src.width(), src.height()),
        wall_width,
        y_plane,
        uv_plane,
        Rect::new(cell.x, cell.y, cell.w, cell.h),
    );
}

/// Scale the `crop` rectangle of `src` into the `dst` rectangle of planes
/// `dst_width` pixels wide, by 2x2 box sampling at nearest source positions
/// (the box never reaches outside `crop`). Pure integer arithmetic, no
/// allocation. `crop` and `dst` are even-aligned (4:2:0); out-of-range reads
/// and writes are skipped, never panic.
pub fn blit_region(
    src: &Nv12Image,
    crop: Rect,
    dst_width: u32,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    dst: Rect,
) {
    // A zero-sized destination or crop draws nothing (and would divide by 0).
    if dst.w == 0 || dst.h == 0 || crop.w == 0 || crop.h == 0 {
        return;
    }
    let sw = src.width();
    let (sy, suv) = (src.y_plane(), src.uv_plane());
    let to = |v: u32| usize::try_from(v).unwrap_or(0);
    let (sw_u, ww) = (to(sw), to(dst_width));
    let (cx, cy, cw, ch) = (to(crop.x), to(crop.y), crop.w, crop.h);
    let last_row = cy + to(ch).saturating_sub(1);
    let last_col = cx + to(cw).saturating_sub(1);
    for row in 0..dst.h {
        let src_row = u64::from(row) * u64::from(ch) / u64::from(dst.h);
        let r0 = cy + to(u32::try_from(src_row).unwrap_or(0));
        let r1 = (r0 + 1).min(last_row);
        let dst_row = to(dst.y + row) * ww;
        for col in 0..dst.w {
            let src_col = cx
                + to(u32::try_from(u64::from(col) * u64::from(cw) / u64::from(dst.w)).unwrap_or(0));
            let c1 = (src_col + 1).min(last_col);
            let px = |r: usize, c: usize| u16::from(sy.get(r * sw_u + c).copied().unwrap_or(16));
            let sum = px(r0, src_col) + px(r0, c1) + px(r1, src_col) + px(r1, c1);
            if let Some(d) = y_plane.get_mut(dst_row + to(dst.x + col)) {
                *d = u8::try_from(sum / 4).unwrap_or(u8::MAX);
            }
        }
    }
    // Chroma: one interleaved Cb/Cr pair per 2x2 luma block.
    let (dcw, dch) = (dst.w / 2, dst.h / 2);
    let (scw, sch) = (cw / 2, ch / 2);
    let (ccx, ccy) = (cx / 2, cy / 2);
    for row in 0..dch {
        let r = ccy
            + to(
                u32::try_from(u64::from(row) * u64::from(sch) / u64::from(dch.max(1))).unwrap_or(0),
            );
        let dst_row = to(dst.y / 2 + row) * ww;
        for col in 0..dcw {
            let c = ccx
                + to(
                    u32::try_from(u64::from(col) * u64::from(scw) / u64::from(dcw.max(1)))
                        .unwrap_or(0),
                );
            let base = r * sw_u + c * 2;
            let d = dst_row + to(dst.x / 2 + col) * 2;
            for k in 0..2 {
                if let (Some(d), Some(s)) = (uv_plane.get_mut(d + k), suv.get(base + k)) {
                    *d = *s;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use multiview_compositor::pipeline::CanvasColor;

    #[test]
    fn a_zero_sized_crop_or_destination_draws_nothing_and_never_panics() {
        let src =
            Nv12Image::solid(16, 16, 200, 90, 60, CanvasColor::default().output_tag()).unwrap();
        let (mut y, mut uv) = (vec![7u8; 16 * 16], vec![7u8; 16 * 8]);
        for (crop, dst) in [
            (Rect::new(0, 0, 16, 16), Rect::new(0, 0, 0, 8)),
            (Rect::new(0, 0, 16, 16), Rect::new(0, 0, 8, 0)),
            (Rect::new(0, 0, 0, 16), Rect::full(16, 16)),
            (Rect::new(0, 0, 16, 0), Rect::full(16, 16)),
        ] {
            blit_region(&src, crop, 16, &mut y, &mut uv, dst);
        }
        assert!(y.iter().all(|&v| v == 7) && uv.iter().all(|&v| v == 7));
    }
}
