//! The enforcement **tile watermark** — a pure, deterministic corner mark
//! appended to the overlay draw list when the entitlement ladder is at a
//! watermark rung (Conspect S3, ADR-0050 §5/§6, brief §6.2).
//!
//! # Never off air (invariant #1)
//!
//! This module **builds primitives**; it holds no engine handle, performs no
//! I/O, and cannot stop, stall, or de-pace output. The watermark is an *overlay
//! convenience* — it rides the existing overlay sub-pass
//! ([`crate::overlay::subpass`]) that bakes off the hot loop, never the tick
//! path. Whether to draw it is decided upstream from a pre-derived ladder flag
//! (an `arc_swap`'d `EnforcementLevel` the cli updates off-thread); this module
//! only renders the mark when asked. Drawing it degrades nothing about the
//! program timing — the canvas still emits one valid frame per tick.
//!
//! # What it draws
//!
//! A small, fixed-shape corner badge built **only** from analytic primitives
//! ([`OverlayPrimitive::FilledRect`] / [`OverlayPrimitive::Line`]) so it needs no
//! font/atlas and is byte-deterministic on CPU and GPU: a translucent dark
//! rounded backing plate in the top-right corner with a bright diagonal slash
//! and a horizontal underline across it. It is intentionally compact and
//! corner-anchored so it marks the **composited multiview canvas** without
//! obscuring tile content — exactly the "corner watermark on multiview TILES
//! only, never the pass-through program" the spec mandates (the multiview canvas
//! *is* the tiled output; a 1:1 pass-through program is never composited through
//! this path, so it is never marked).
//!
//! The mark is region-limited: every primitive footprint is in the corner, so
//! the region-limited bake ([`crate::overlay::subpass::apply_overlays_to_nv12`])
//! only colour-round-trips the corner, keeping the per-frame cost bounded.

use crate::overlay::subpass::{OverlayColor, OverlayDrawList, OverlayPrimitive, OverlayRect};

/// The fraction of the shorter canvas side the watermark badge spans. A small,
/// corner-anchored mark (≈11% of the short side) — visible but unobtrusive.
const BADGE_FRACTION: u32 = 9;

/// The minimum badge size (px) so the mark stays legible on a small canvas.
const MIN_BADGE_PX: u32 = 28;

/// The maximum badge size (px) so the mark stays a corner mark on a huge canvas.
const MAX_BADGE_PX: u32 = 96;

/// The inset of the badge from the canvas edge, as a fraction of the badge size.
const INSET_FRACTION: u32 = 4;

/// A translucent dark backing plate, so the mark reads over any tile content
/// beneath it (the meaning is the *mark*, the plate is contrast only).
const PLATE: OverlayColor = OverlayColor::new(0.0, 0.0, 0.0, 0.55);

/// The bright mark stroke colour (a warm amber, distinct from any tile chrome).
const MARK: OverlayColor = OverlayColor::new(0.96, 0.74, 0.12, 0.92);

/// Build the watermark primitives for a `width × height` canvas, anchored in the
/// **top-right** corner, and append them to `list` (drawn last, on top).
///
/// Pure + deterministic: the same canvas geometry always yields the same
/// primitives. Returns the number of primitives appended (always ≥ 1 for a
/// non-degenerate canvas) so a caller/test can assert the mark was emitted.
///
/// A degenerate (zero-extent) canvas appends nothing and returns `0` — never a
/// panic.
#[must_use]
pub fn push_tile_watermark(list: &mut OverlayDrawList, width: u32, height: u32) -> usize {
    if width == 0 || height == 0 {
        return 0;
    }
    let short = width.min(height);
    let badge = (short / BADGE_FRACTION)
        .clamp(MIN_BADGE_PX, MAX_BADGE_PX)
        .min(short);
    if badge == 0 {
        return 0;
    }
    let inset = (badge / INSET_FRACTION).max(2);
    // Top-right anchor: the badge's left edge is `badge + inset` in from the
    // right; the top edge is `inset` down from the top. Saturating so a canvas
    // narrower than the badge still produces an on-canvas (clamped) mark.
    let left = i64::from(width)
        .saturating_sub(i64::from(badge))
        .saturating_sub(i64::from(inset));
    let left = i32_clamp(left);
    let top = i32_clamp(i64::from(inset));

    let before = list.len();

    // 1. The translucent backing plate (rounded corners), so the mark reads over
    //    whatever tile content sits beneath it.
    let corner_radius = (badge / 6).max(1);
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(left, top, badge, badge),
        corner_radius,
        color: PLATE,
    });

    // 2. A bright diagonal slash across the plate (top-left → bottom-right),
    //    drawn as a thick angled capsule so it is recognisable as a deliberate
    //    mark rather than tile chrome.
    let pad = (badge / 6).max(1);
    let half_thickness = unit(badge.max(1)) * 0.06_f32 + 1.0;
    let x0 = unit_signed(left) + unit(pad);
    let y0 = unit_signed(top) + unit(pad);
    let x1 = unit_signed(left) + unit(badge.saturating_sub(pad));
    let y1 = unit_signed(top) + unit(badge.saturating_sub(pad));
    list.push(OverlayPrimitive::Stroke {
        x0,
        y0,
        x1,
        y1,
        half_thickness,
        color: MARK,
    });

    // 3. A horizontal underline bar near the bottom of the plate, distinguishing
    //    the watermark from a plain decorative diagonal.
    let bar_h = (badge / 10).max(2);
    let bar_w = badge.saturating_sub(pad.saturating_mul(2)).max(1);
    let bar_y = top.saturating_add(i32_clamp(i64::from(
        badge.saturating_sub(pad).saturating_sub(bar_h),
    )));
    list.push(OverlayPrimitive::Line {
        rect: OverlayRect::new(
            left.saturating_add(i32_clamp(i64::from(pad))),
            bar_y,
            bar_w,
            bar_h,
        ),
        color: MARK,
    });

    list.len().saturating_sub(before)
}

/// Saturating `i64 → i32` (the canvas/badge maths stay well within range).
fn i32_clamp(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Exact small-`u32` → `f32` (badge sizes are well under `2^24`), no `as`.
fn unit(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Exact small-`i32` → `f32`, no `as`.
fn unit_signed(value: i32) -> f32 {
    if value < 0 {
        -unit(value.unsigned_abs())
    } else {
        unit(u32::try_from(value).unwrap_or(u32::MAX))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::overlay::subpass::OverlayDrawList;

    /// The watermark emits a backing plate + a mark stroke + an underline into
    /// the TOP-RIGHT corner of the canvas, and nothing anywhere else.
    #[test]
    fn watermark_marks_the_top_right_corner() {
        let (w, h) = (1280, 720);
        let mut list = OverlayDrawList::new();
        let n = push_tile_watermark(&mut list, w, h);
        assert!(
            n >= 3,
            "watermark emits a plate + slash + underline, got {n}"
        );
        assert_eq!(list.len(), n, "appends exactly its own primitives");

        // Every primitive footprint must sit in the right half + top half of the
        // canvas (a corner mark, never canvas-wide).
        for prim in &list.primitives {
            let (px, py) = primitive_top_left(prim);
            assert!(
                px >= i32::try_from(w / 2).unwrap(),
                "watermark primitive must be in the right half (x={px})"
            );
            assert!(
                py < i32::try_from(h / 2).unwrap(),
                "watermark primitive must be in the top half (y={py})"
            );
        }
    }

    /// A degenerate canvas appends nothing (never panics).
    #[test]
    fn a_zero_canvas_emits_nothing() {
        let mut list = OverlayDrawList::new();
        assert_eq!(push_tile_watermark(&mut list, 0, 720), 0);
        assert_eq!(push_tile_watermark(&mut list, 1280, 0), 0);
        assert!(list.is_empty());
    }

    /// The mark is deterministic: the same geometry yields the identical list.
    #[test]
    fn watermark_is_deterministic() {
        let mut a = OverlayDrawList::new();
        let mut b = OverlayDrawList::new();
        let _ = push_tile_watermark(&mut a, 1920, 1080);
        let _ = push_tile_watermark(&mut b, 1920, 1080);
        assert_eq!(a.primitives, b.primitives);
    }

    /// The badge size is bounded so it stays a corner mark even on a 4K canvas.
    #[test]
    fn the_badge_stays_a_corner_mark_on_a_large_canvas() {
        let mut list = OverlayDrawList::new();
        let _ = push_tile_watermark(&mut list, 3840, 2160);
        let plate = list
            .primitives
            .iter()
            .find_map(|p| match p {
                OverlayPrimitive::FilledRect { rect, .. } => Some(*rect),
                _ => None,
            })
            .expect("a backing plate");
        assert!(
            plate.width <= MAX_BADGE_PX && plate.height <= MAX_BADGE_PX,
            "badge stays bounded: {}x{}",
            plate.width,
            plate.height
        );
    }

    fn primitive_top_left(prim: &OverlayPrimitive) -> (i32, i32) {
        match prim {
            OverlayPrimitive::FilledRect { rect, .. } | OverlayPrimitive::Line { rect, .. } => {
                (rect.x, rect.y)
            }
            OverlayPrimitive::Stroke { x0, y0, x1, y1, .. } => {
                (floor_to_i32(x0.min(*x1)), floor_to_i32(y0.min(*y1)))
            }
            other => panic!("unexpected watermark primitive {other:?}"),
        }
    }

    /// Floor a small `f32` to `i32` for the corner assertions, no `as` cast.
    fn floor_to_i32(v: f32) -> i32 {
        if v < 0.0 {
            return 0;
        }
        let mut n: i32 = 0;
        while f32::from(u16::try_from(n).unwrap_or(0)) + 1.0 <= v && n < i32::from(u16::MAX) {
            n += 1;
        }
        n
    }
}
