//! Keyer coverage and scene groups for the one Program compose pass
//! (ADR-0113 §2-§4, design note `docs/architecture/switcher-keyers.md` §3).
//!
//! A keyer is one ordinary [`crate::pipeline::Tile`] with a [`TileKey`]: its
//! per-pixel coverage is
//!
//! ```text
//! a = opacity × mask(sx, sy) × fill_alpha(sx, sy) × matte(kx, ky)
//! ```
//!
//! where `opacity` already carries the keyer's on-air weight (the engine
//! multiplies it in), `fill_alpha` is the fill's own NV12+A plane (applied by
//! both kernels since ADR-0058), and `matte` comes from the key type. Each
//! factor is a straight alpha in `[0, 1]`; their product is premultiplied once,
//! at the existing step (invariant #8 order untouched). The matte reads only
//! post-range-expand luma (pipeline step 1), so a key never enters the colour
//! math of steps 2-5.
//!
//! A background transition that changes the key set mixes two *scenes* exactly
//! (ADR-0113 §2): each tile carries a [`TileGroup`]. Layers tagged
//! [`SceneMember::Both`], [`SceneMember::Out`] or [`SceneMember::In`] fold into
//! the outgoing and/or incoming accumulator; at the first following
//! [`TileGroup::Top`] tile (or the end of the list) the two accumulators are
//! merged with the segment's [`SceneMerge`]. `Top` layers fold once onto the
//! merged result, which is exact because `over` is linear in what lies
//! beneath.

use crate::blend::{LinearRgba, PremulRgba};
use crate::error::Result;
use crate::pipeline::Nv12Image;
use crate::range;

/// How a keyer derives its matte (ADR-0113 §3). Chroma keys are slice K7.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum KeyKind {
    /// Linear key. With no key source the matte is 1 (the fill's own alpha
    /// shapes it; an opaque fill is a box). With a key source the matte is the
    /// key's post-range-expand Y, clamped to `[0, 1]` (a fill and key pair).
    Linear,
    /// Luma key: `clamp((y_norm − clip) × gain, 0, 1)` on post-range-expand Y
    /// of the key source, or of the fill when there is none.
    Luma {
        /// Clip level on normalized Y (`0` = black, `1` = nominal white).
        clip: f32,
        /// Gain (slope) applied above the clip; `>= 0`.
        gain: f32,
    },
}

/// A rectangle in **fill-source pixels** outside which the key is off
/// (ADR-0113 §3). Because it is in source space it travels with a flying key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyMask {
    /// Left column (source pixels).
    pub x: u32,
    /// Top row (source pixels).
    pub y: u32,
    /// Width (source pixels).
    pub w: u32,
    /// Height (source pixels).
    pub h: u32,
}

impl KeyMask {
    /// Whether source pixel `(sx, sy)` lies inside the mask (half-open, u64
    /// arithmetic so a huge rectangle cannot overflow).
    #[must_use]
    pub fn contains(&self, sx: u32, sy: u32) -> bool {
        let x1 = u64::from(self.x) + u64::from(self.w);
        let y1 = u64::from(self.y) + u64::from(self.h);
        sx >= self.x && sy >= self.y && u64::from(sx) < x1 && u64::from(sy) < y1
    }
}

/// The key block of a keyer tile (ADR-0113 §3).
#[derive(Debug, Clone, Copy)]
pub struct TileKey<'a> {
    /// Matte derivation.
    pub kind: KeyKind,
    /// Separate key signal (its Y is the matte), or `None` for a self key.
    /// Sampled independently of the fill: `kx = floor(ddx × key_w / dst_w)`.
    pub source: Option<&'a Nv12Image>,
    /// Invert the matte (`1 − m`). Applies whenever the matte comes from a
    /// signal (luma, or linear with a key source); a linear self key has no
    /// signal and ignores it.
    pub invert: bool,
    /// Optional mask in fill-source pixels.
    pub mask: Option<KeyMask>,
}

impl<'a> TileKey<'a> {
    /// A linear key with no key source: the fill's own alpha shapes it.
    #[must_use]
    pub const fn linear() -> Self {
        Self {
            kind: KeyKind::Linear,
            source: None,
            invert: false,
            mask: None,
        }
    }

    /// A luma key on the fill itself (or on `source` once set).
    #[must_use]
    pub const fn luma(clip: f32, gain: f32) -> Self {
        Self {
            kind: KeyKind::Luma { clip, gain },
            source: None,
            invert: false,
            mask: None,
        }
    }

    /// Use `source`'s luma as the key signal.
    #[must_use]
    pub const fn with_source(mut self, source: &'a Nv12Image) -> Self {
        self.source = Some(source);
        self
    }

    /// Set the invert flag.
    #[must_use]
    pub const fn inverted(mut self, invert: bool) -> Self {
        self.invert = invert;
        self
    }

    /// Set the source-space mask.
    #[must_use]
    pub const fn masked(mut self, mask: KeyMask) -> Self {
        self.mask = Some(mask);
        self
    }
}

/// Which scene a grouped layer belongs to (ADR-0113 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneMember {
    /// In both scenes (shared background of a key-only transition, an unarmed
    /// on-air USK below a changing one): folded into both accumulators.
    Both,
    /// Outgoing scene only (outgoing background, USKs going off).
    Out,
    /// Incoming scene only (incoming background, USKs going on).
    In,
}

/// How the outgoing and incoming scene accumulators are merged, per pixel, in
/// premultiplied linear light (ADR-0113 §2). Wipe arrives with slice K3 on
/// top of lane `transitions`' mask.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum SceneMerge {
    /// `lerp(out, in, p)`.
    Mix {
        /// Transition progress in `[0, 1]`.
        p: f32,
    },
    /// `lerp(lerp(out, colour, d), in, i)` with lane `transitions`'
    /// `dip_weights`.
    Dip {
        /// Opaque dip colour, linear in the canvas gamut.
        colour: LinearRgba,
        /// Outgoing-to-colour weight in `[0, 1]`.
        d: f32,
        /// Colour-to-incoming weight in `[0, 1]` (0 until the switch point).
        i: f32,
    },
}

impl SceneMerge {
    /// Merge one pixel's two scene accumulators.
    #[must_use]
    pub fn apply(self, out: PremulRgba, inc: PremulRgba) -> PremulRgba {
        match self {
            Self::Mix { p } => lerp(out, inc, unit(p)),
            Self::Dip { colour, d, i } => {
                let colour = LinearRgba { a: 1.0, ..colour }.premultiplied();
                lerp(lerp(out, colour, unit(d)), inc, unit(i))
            }
        }
    }
}

/// The scene group of one tile (ADR-0113 §2). [`TileGroup::Top`] is the
/// default: an ordinary layer folded once onto whatever lies beneath.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum TileGroup {
    /// Folded once onto the (merged) result. Every non-keyer tile, every DSK,
    /// and unarmed on-air USKs above the highest changing layer.
    #[default]
    Top,
    /// A member of a scene being transitioned; folded into the accumulator(s)
    /// of its scene(s), merged with `merge` before the next `Top` layer.
    Scene {
        /// Which scene(s).
        member: SceneMember,
        /// The segment's merge. Every grouped tile of one segment carries the
        /// same merge; when they differ the last grouped tile's merge wins.
        merge: SceneMerge,
    },
}

impl TileGroup {
    /// The merge of a grouped tile, or `None` for `Top`.
    #[must_use]
    pub const fn merge(self) -> Option<SceneMerge> {
        match self {
            Self::Top => None,
            Self::Scene { merge, .. } => Some(merge),
        }
    }
}

/// Clamp to `[0, 1]`, mapping NaN to 0 so a bad weight can never poison a
/// pixel.
#[must_use]
pub(crate) fn unit(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// Component-wise `a × (1 − t) + b × t`.
fn lerp(a: PremulRgba, b: PremulRgba, t: f32) -> PremulRgba {
    let s = 1.0 - t;
    PremulRgba {
        r: a.r.mul_add(s, b.r * t),
        g: a.g.mul_add(s, b.g * t),
        b: a.b.mul_add(s, b.b * t),
        a: a.a.mul_add(s, b.a * t),
    }
}

/// Nearest-neighbour index of destination offset `d` (in `[0, dst)`) on a
/// source axis of `src` samples: `floor(d × src / dst)`, clamped. Mirrors the
/// fill's mapping so fill and key each use their own size.
fn axis(d: u32, src: u32, dst: u32) -> u32 {
    if dst == 0 || src == 0 {
        return 0;
    }
    let idx = u64::from(d).saturating_mul(u64::from(src)) / u64::from(dst);
    u32::try_from(idx.min(u64::from(src - 1))).unwrap_or(src - 1)
}

/// The luma-key matte `clamp((y_norm − clip) × gain, 0, 1)` before invert.
/// A negative or NaN gain reads as 0, so the matte is monotonic
/// non-decreasing in `y_norm` and total for every input.
#[must_use]
pub fn luma_matte(y_norm: f32, clip: f32, gain: f32) -> f32 {
    let gain = if gain.is_nan() { 0.0 } else { gain.max(0.0) };
    unit((y_norm - clip) * gain)
}

/// Post-range-expand luma of `image` at `(x, y)`, or `None` out of bounds.
fn luma_at(image: &Nv12Image, x: u32, y: u32) -> Result<Option<f32>> {
    let key_range = range::require_resolved(image.color().range)?;
    let index = usize::try_from(y)
        .ok()
        .zip(usize::try_from(image.width()).ok())
        .and_then(|(row, w)| row.checked_mul(w))
        .and_then(|base| base.checked_add(usize::try_from(x).ok()?));
    Ok(index
        .and_then(|i| image.y_plane().get(i))
        .map(|&y8| range::expand_luma(y8, key_range)))
}

/// The straight-alpha key factor `mask × matte` for one destination pixel of
/// a keyed tile (ADR-0113 §3). `fill_y` is the fill's post-range-expand Y at
/// `(sx, sy)` (used by a luma self key); `(ddx, ddy)` is the destination-rect
/// offset and `(dst_w, dst_h)` the destination size, from which the key
/// source is sampled at its own resolution.
///
/// # Errors
///
/// [`crate::Error::UnresolvedColor`] when the key source's range is
/// unresolved.
#[allow(clippy::too_many_arguments)]
// reason: one pixel's sampling context (fill coords, destination offset and
// size, fill luma); a struct would be built per pixel for no gain.
pub(crate) fn key_factor(
    key: &TileKey<'_>,
    sx: u32,
    sy: u32,
    ddx: u32,
    ddy: u32,
    dst_w: u32,
    dst_h: u32,
    fill_y: f32,
) -> Result<f32> {
    if let Some(mask) = key.mask {
        if !mask.contains(sx, sy) {
            return Ok(0.0);
        }
    }
    let signal = match key.source {
        Some(src) => {
            let kx = axis(ddx, src.width(), dst_w);
            let ky = axis(ddy, src.height(), dst_h);
            luma_at(src, kx, ky)?
        }
        None => None,
    };
    let matte = match key.kind {
        KeyKind::Linear => match signal {
            None => return Ok(1.0),
            Some(y) => unit(y),
        },
        KeyKind::Luma { clip, gain } => luma_matte(signal.unwrap_or(fill_y), clip, gain),
    };
    Ok(if key.invert { 1.0 - matte } else { matte })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_samples_each_size_independently() {
        // Design note §3.1: fill width 2, key width 4, destination 4 samples
        // key columns 0, 1, 2, 3 (and fill columns 0, 0, 1, 1).
        let key: Vec<u32> = (0..4).map(|d| axis(d, 4, 4)).collect();
        let fill: Vec<u32> = (0..4).map(|d| axis(d, 2, 4)).collect();
        assert_eq!(key, vec![0, 1, 2, 3]);
        assert_eq!(fill, vec![0, 0, 1, 1]);
    }

    #[test]
    fn mask_is_half_open_and_overflow_safe() {
        let m = KeyMask {
            x: 2,
            y: 2,
            w: 2,
            h: u32::MAX,
        };
        assert!(m.contains(2, 2));
        assert!(m.contains(3, u32::MAX - 1));
        assert!(!m.contains(4, 2));
        assert!(!m.contains(1, 2));
    }

    #[test]
    fn mix_merge_of_identical_scenes_is_identity_at_any_p() {
        let x = PremulRgba {
            r: 0.25,
            g: 0.5,
            b: 0.125,
            a: 1.0,
        };
        for p in [0.0, 0.3, 0.5, 1.0] {
            let m = SceneMerge::Mix { p }.apply(x, x);
            assert!((m.r - x.r).abs() < 1e-6 && (m.a - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn nan_weights_are_treated_as_zero() {
        assert_eq!(unit(f32::NAN), 0.0);
        assert_eq!(unit(-1.0), 0.0);
        assert_eq!(unit(2.0), 1.0);
    }
}
