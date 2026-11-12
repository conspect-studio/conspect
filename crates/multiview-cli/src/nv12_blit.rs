//! Pure-integer NV12 cell scaler shared by the monitor wall (ADR-0103) and the
//! routing aux canvases (ADR-0108 §2.4). Colour samples are copied, never
//! converted: the caller guarantees the source and destination share a tag.

use multiview_compositor::pipeline::Nv12Image;
use multiview_control::preview::WallCell;

/// Scale `src` into the rectangle `cell` of the wall planes by 2x2 box sampling
/// at nearest source positions. Pure integer arithmetic, no allocation.
pub fn blit_cell(
    src: &Nv12Image,
    wall_width: u32,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    cell: &WallCell,
) {
    let (sw, sh) = (src.width(), src.height());
    let (sy, suv) = (src.y_plane(), src.uv_plane());
    let to = |v: u32| usize::try_from(v).unwrap_or(0);
    let (sw_u, ww) = (to(sw), to(wall_width));
    for row in 0..cell.h {
        let src_row = u64::from(row) * u64::from(sh) / u64::from(cell.h);
        let r0 = to(u32::try_from(src_row).unwrap_or(0));
        let r1 = (r0 + 1).min(to(sh).saturating_sub(1));
        let dst_row = to(cell.y + row) * ww;
        for col in 0..cell.w {
            let src_col =
                to(u32::try_from(u64::from(col) * u64::from(sw) / u64::from(cell.w)).unwrap_or(0));
            let c1 = (src_col + 1).min(sw_u.saturating_sub(1));
            let px = |r: usize, c: usize| u16::from(sy.get(r * sw_u + c).copied().unwrap_or(16));
            let sum = px(r0, src_col) + px(r0, c1) + px(r1, src_col) + px(r1, c1);
            if let Some(dst) = y_plane.get_mut(dst_row + to(cell.x + col)) {
                *dst = u8::try_from(sum / 4).unwrap_or(u8::MAX);
            }
        }
    }
    // Chroma: one interleaved Cb/Cr pair per 2x2 luma block.
    let (cw, ch) = (cell.w / 2, cell.h / 2);
    let (scw, sch) = (sw / 2, sh / 2);
    for row in 0..ch {
        let r =
            to(u32::try_from(u64::from(row) * u64::from(sch) / u64::from(ch.max(1))).unwrap_or(0));
        let dst_row = to(cell.y / 2 + row) * ww;
        for col in 0..cw {
            let c = to(
                u32::try_from(u64::from(col) * u64::from(scw) / u64::from(cw.max(1))).unwrap_or(0),
            );
            let base = r * sw_u + c * 2;
            let dst = dst_row + to(cell.x / 2 + col) * 2;
            for k in 0..2 {
                if let (Some(d), Some(s)) = (uv_plane.get_mut(dst + k), suv.get(base + k)) {
                    *d = *s;
                }
            }
        }
    }
}
