//! Pure-integer fit maths for a per-output format (#342, output-routing
//! §12.3/§12.4): where a picture of one size lands in an output of another,
//! for `contain` (letter/pillarbox at an anchor), `cover` (crop to the
//! output's aspect at an anchor, "zoom and clip") and `fill` (stretch).
//!
//! Every origin and size is even (4:2:0 chroma) and inside its plane.

use multiview_config::{Anchor, OutputFitMode};

/// An even-aligned pixel rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

impl Rect {
    /// A rectangle.
    #[must_use]
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// The whole of a `w × h` plane, sizes floored to even.
    #[must_use]
    pub const fn full(w: u32, h: u32) -> Self {
        Self::new(0, 0, w & !1, h & !1)
    }
}

/// `free` space split by an anchor half (`0` start, `1` middle, `2` end),
/// floored to even.
fn place(free: u32, half: u8) -> u32 {
    let v = u64::from(free) * u64::from(half) / 2;
    u32::try_from(v).unwrap_or(0) & !1
}

/// `num / den` rounded to the nearest even integer (ties up), at least 2.
fn round_even(num: u64, den: u64) -> u64 {
    (((num + den) / (2 * den)) * 2).max(2)
}

/// The source crop and destination rectangle that bring a `src_w × src_h`
/// picture into a `dst_w × dst_h` output with `mode` at `anchor`.
///
/// - contain: the whole source into the largest even rectangle of its aspect
///   inside the output, placed at the anchor (centre equals the routing aux's
///   `contain_rect`);
/// - cover: the destination is the whole output; the source crop is the
///   largest even rectangle of the output's aspect inside the source, placed
///   at the anchor;
/// - fill: the whole source onto the whole output.
///
/// `None` when either size is below 2 pixels on an axis.
#[must_use]
pub fn fit_rects(
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    mode: OutputFitMode,
    anchor: Anchor,
) -> Option<(Rect, Rect)> {
    if src_w < 2 || src_h < 2 || dst_w < 2 || dst_h < 2 {
        return None;
    }
    let (sw, sh, dw, dh) = (
        u64::from(src_w),
        u64::from(src_h),
        u64::from(dst_w),
        u64::from(dst_h),
    );
    let (ax, ay) = anchor.halves();
    let src_full = Rect::full(src_w, src_h);
    let dst_full = Rect::full(dst_w, dst_h);
    match mode {
        OutputFitMode::Fill => Some((src_full, dst_full)),
        OutputFitMode::Cover => {
            // Source wider than the output's aspect: crop the width.
            let (cw, ch) = if sw * dh >= sh * dw {
                (round_even(sh * dw, dh).min(sw & !1), sh & !1)
            } else {
                (sw & !1, round_even(sw * dh, dw).min(sh & !1))
            };
            let (cw, ch) = (u32::try_from(cw).ok()?, u32::try_from(ch).ok()?);
            let x = place(src_w - cw, ax);
            let y = place(src_h - ch, ay);
            Some((Rect::new(x, y, cw, ch), dst_full))
        }
        // Contain (and any later mode until it is taught here).
        _ => {
            let (w, h) = if sw * dh >= sh * dw {
                (dw, sh * dw / sw)
            } else {
                (sw * dh / sh, dh)
            };
            let even = |v: u64| u32::try_from(v & !1).ok();
            let (w, h) = (even(w)?.max(2), even(h)?.max(2));
            let x = place(dst_w - w, ax);
            let y = place(dst_h - h, ay);
            Some((src_full, Rect::new(x, y, w, h)))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::nv12_blit::blit_region;
    use crate::routing_aux::contain_rect;
    use multiview_compositor::pipeline::{CanvasColor, Nv12Image};

    const MODES: [OutputFitMode; 3] = [
        OutputFitMode::Contain,
        OutputFitMode::Cover,
        OutputFitMode::Fill,
    ];

    fn in_bounds(r: Rect, w: u32, h: u32) -> bool {
        r.w >= 2 && r.h >= 2 && r.x + r.w <= w && r.y + r.h <= h
    }

    fn even(r: Rect) -> bool {
        (r.x | r.y | r.w | r.h) & 1 == 0
    }

    #[test]
    fn vertical_cover_centre_crops_the_middle_column() {
        let (src, dst) =
            fit_rects(1920, 1080, 1080, 1920, OutputFitMode::Cover, Anchor::Center).unwrap();
        assert_eq!(src, Rect::new(656, 0, 608, 1080));
        assert_eq!(dst, Rect::new(0, 0, 1080, 1920));
    }

    #[test]
    fn contain_centre_equals_the_aux_contain_rect() {
        for (sw, sh) in [(640, 360), (640, 480), (1920, 800), (641, 481), (3, 2000)] {
            for (dw, dh) in [(1920, 1080), (1080, 1920), (1280, 720), (642, 482)] {
                let (src, dst) =
                    fit_rects(sw, sh, dw, dh, OutputFitMode::Contain, Anchor::Center).unwrap();
                let (x, y, w, h) = contain_rect(sw, sh, dw, dh).unwrap();
                assert_eq!(dst, Rect::new(x, y, w, h), "{sw}x{sh} in {dw}x{dh}");
                assert_eq!(src, Rect::full(sw, sh));
            }
        }
    }

    #[test]
    fn four_by_three_into_sixteen_by_nine() {
        let contain = fit_rects(640, 480, 1920, 1080, OutputFitMode::Contain, Anchor::Center);
        assert_eq!(
            contain,
            Some((Rect::new(0, 0, 640, 480), Rect::new(240, 0, 1440, 1080)))
        );
        let cover = fit_rects(640, 480, 1920, 1080, OutputFitMode::Cover, Anchor::Center);
        // 640 * 9 / 16 = 360 rows, centred: (480 - 360) / 2 = 60.
        assert_eq!(
            cover,
            Some((Rect::new(0, 60, 640, 360), Rect::new(0, 0, 1920, 1080)))
        );
        let fill = fit_rects(640, 480, 1920, 1080, OutputFitMode::Fill, Anchor::Center);
        assert_eq!(
            fill,
            Some((Rect::new(0, 0, 640, 480), Rect::new(0, 0, 1920, 1080)))
        );
    }

    #[test]
    fn every_anchor_places_at_its_edge() {
        let expect = |a: Anchor| -> (u32, u32) {
            let (hx, hy) = a.halves();
            (u32::from(hx) * 656, u32::from(hy) * 0)
        };
        for anchor in Anchor::ALL {
            // Cover: the crop moves along the free width (1920 - 608 = 1312).
            let (src, _) = fit_rects(1920, 1080, 1080, 1920, OutputFitMode::Cover, anchor).unwrap();
            assert_eq!((src.x, src.y), expect(anchor), "{anchor:?}");
            // Contain: the box moves along the free space.
            let (_, dst) = fit_rects(640, 480, 1920, 1080, OutputFitMode::Contain, anchor).unwrap();
            let (hx, _) = anchor.halves();
            assert_eq!(dst.x, u32::from(hx) * 240, "{anchor:?}");
            assert_eq!(dst.y, 0);
            let (_, dst) =
                fit_rects(1920, 800, 1920, 1080, OutputFitMode::Contain, anchor).unwrap();
            let (_, hy) = anchor.halves();
            assert_eq!(dst.y, u32::from(hy) * 140, "{anchor:?}");
            // Fill ignores the anchor.
            let fill = fit_rects(640, 480, 1920, 1080, OutputFitMode::Fill, anchor).unwrap();
            assert_eq!(fill.1, Rect::full(1920, 1080));
        }
    }

    #[test]
    fn odd_size_sweep_is_even_and_in_bounds() {
        for sw in (2..2100).step_by(37) {
            for sh in (2..1300).step_by(41) {
                for (dw, dh) in [(1920, 1080), (1080, 1920), (16, 16), (642, 482), (2, 2)] {
                    for mode in MODES {
                        for anchor in Anchor::ALL {
                            let (src, dst) = fit_rects(sw, sh, dw, dh, mode, anchor).unwrap();
                            assert!(even(src) && even(dst), "{sw}x{sh}->{dw}x{dh} {mode:?}");
                            assert!(in_bounds(src, sw, sh), "src {sw}x{sh}->{dw}x{dh} {mode:?}");
                            assert!(in_bounds(dst, dw, dh), "dst {sw}x{sh}->{dw}x{dh} {mode:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn tiny_sizes() {
        for mode in MODES {
            assert!(fit_rects(1, 4, 16, 16, mode, Anchor::Center).is_none());
            assert!(fit_rects(4, 4, 1, 16, mode, Anchor::Center).is_none());
            let (src, dst) = fit_rects(2, 2, 2, 2, mode, Anchor::Center).unwrap();
            assert_eq!((src, dst), (Rect::full(2, 2), Rect::full(2, 2)));
            let (src, dst) = fit_rects(2, 1000, 16, 16, mode, Anchor::BottomRight).unwrap();
            assert!(in_bounds(src, 2, 1000) && in_bounds(dst, 16, 16));
        }
    }

    fn gradient(w: u32, h: u32) -> Nv12Image {
        let (wu, hu) = (usize::try_from(w).unwrap(), usize::try_from(h).unwrap());
        let y = (0..wu * hu)
            .map(|i| u8::try_from(16 + (i % wu * 3 + i / wu * 5) % 219).unwrap())
            .collect();
        let uv = (0..wu * hu / 2)
            .map(|i| u8::try_from(16 + (i * 3) % 224).unwrap())
            .collect();
        Nv12Image::new(w, h, y, uv, CanvasColor::default().output_tag()).unwrap()
    }

    #[test]
    fn cover_centre_of_a_gradient_is_the_expected_crop() {
        let src = gradient(320, 180);
        let (crop, dst) =
            fit_rects(320, 180, 108, 192, OutputFitMode::Cover, Anchor::Center).unwrap();
        // 180 * 108 / 192 = 101.25 → 102 wide, x = (320 - 102) / 2 = 109 → 108.
        assert_eq!(crop, Rect::new(108, 0, 102, 180));
        // Cut the expected crop out as its own image, then scale it whole.
        let (cw, ch) = (crop.w as usize, crop.h as usize);
        let (sx, sy, sw) = (crop.x as usize, crop.y as usize, 320usize);
        let mut cy = Vec::with_capacity(cw * ch);
        for r in 0..ch {
            cy.extend_from_slice(&src.y_plane()[(sy + r) * sw + sx..(sy + r) * sw + sx + cw]);
        }
        let mut cuv = Vec::with_capacity(cw * ch / 2);
        for r in 0..ch / 2 {
            let start = (sy / 2 + r) * sw + sx;
            cuv.extend_from_slice(&src.uv_plane()[start..start + cw]);
        }
        let cut = Nv12Image::new(crop.w, crop.h, cy, cuv, src.color()).unwrap();
        let (dw, dh) = (108usize, 192usize);
        let (mut ay, mut auv) = (vec![0u8; dw * dh], vec![0u8; dw * dh / 2]);
        blit_region(&src, crop, 108, &mut ay, &mut auv, dst);
        let (mut by, mut buv) = (vec![0u8; dw * dh], vec![0u8; dw * dh / 2]);
        blit_region(
            &cut,
            Rect::full(crop.w, crop.h),
            108,
            &mut by,
            &mut buv,
            dst,
        );
        assert!(
            ay == by,
            "luma of the crop-aware scale equals the cut-out scale"
        );
        assert!(
            auv == buv,
            "chroma of the crop-aware scale equals the cut-out scale"
        );
    }
}
