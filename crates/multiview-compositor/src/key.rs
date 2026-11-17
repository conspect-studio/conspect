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
//! math of steps 2-5. A chroma key (slice K7, ADR-C007 §3) reads the fill's
//! **tile-gamut linear RGB** (after step 3, before the step-4 primaries
//! convert) — a value both kernels already compute — and only multiplies the
//! result into alpha at the same single premultiply, so the step order is
//! untouched.
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
use crate::mask::WipeMask;
use crate::pipeline::Nv12Image;
use crate::range;

/// How a keyer derives its matte (ADR-0113 §3, ADR-C007 §3).
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
    /// Chroma key (slice K7, ADR-C007 §3): `clamp((d − clip) × gain, 0, 1)`
    /// where `d` is the Euclidean distance, in the fill's **own-gamut linear
    /// RGB** (after its EOTF, before the primaries convert), between the
    /// fill pixel and the reference colour. The reference (full-range R'G'B'
    /// codes) is quantized once per frame onto the fill's own `(Y, Cb, Cr)`
    /// code grid and decoded through the same front half as the pixels
    /// ([`chroma_reference_linear`]), so a pixel carrying those codes is at
    /// distance exactly 0 and keyed out (matte 0) on every path. Chroma keys the fill itself:
    /// a key source is ignored (config refuses one).
    Chroma {
        /// Reference colour as gamma-encoded full-range R'G'B' codes of the
        /// fill's own transfer (`#00b140` = `[0x00, 0xb1, 0x40]`).
        reference: [u8; 3],
        /// Distance below which the pixel is fully keyed out (linear RGB
        /// units; `0` = only the exact reference).
        clip: f32,
        /// Gain (slope) of the soft edge above the clip; `>= 0`.
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

    /// A chroma key on the fill's own colour (ADR-C007 §3).
    #[must_use]
    pub const fn chroma(reference: [u8; 3], clip: f32, gain: f32) -> Self {
        Self {
            kind: KeyKind::Chroma {
                reference,
                clip,
                gain,
            },
            source: None,
            invert: false,
            mask: None,
        }
    }

    /// Use `source`'s luma as the key signal (ignored by a chroma key).
    #[must_use]
    pub const fn with_source(mut self, source: &'a Nv12Image) -> Self {
        self.source = Some(source);
        self
    }

    /// The separate key source the matte reads, if any. A chroma key keys
    /// the fill's own colour, so it never reads (or uploads) one.
    #[must_use]
    pub const fn signal_source(&self) -> Option<&'a Nv12Image> {
        match self.kind {
            KeyKind::Chroma { .. } => None,
            _ => self.source,
        }
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
/// premultiplied linear light (ADR-0113 §2). Wipe (slice K3) uses lane
/// `transitions`' [`WipeMask`] at each canvas pixel.
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
    /// `lerp(out, in, m(x, y))` with the WIPE mask of lane `transitions` at
    /// canvas pixel `(x, y)` (ADR-0113 §3.2, slice K3). A mask that is not
    /// valid for the canvas ([`WipeMask::is_valid_for`]) reads 0 everywhere:
    /// the outgoing scene, never a poisoned pixel.
    Wipe {
        /// The incoming picture's mask at this step (no border band).
        mask: WipeMask,
    },
}

impl SceneMerge {
    /// Merge one pixel's two scene accumulators at canvas pixel `px` of a
    /// `canvas_wh` canvas (only a WIPE depends on the position).
    #[must_use]
    pub fn apply(
        self,
        out: PremulRgba,
        inc: PremulRgba,
        px: (u32, u32),
        canvas_wh: (u32, u32),
    ) -> PremulRgba {
        match self {
            Self::Mix { p } => lerp(out, inc, unit(p)),
            Self::Wipe { mask } => {
                let m = if mask.is_valid_for(canvas_wh.0, canvas_wh.1) {
                    unit(mask.coverage(px.0, px.1))
                } else {
                    0.0
                };
                lerp(out, inc, m)
            }
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

/// A subnormal clip reads as 0, so a GPU that flushes subnormals sees the
/// same parameter as the oracle.
#[must_use]
pub fn flush_subnormal(v: f32) -> f32 {
    if v.is_subnormal() {
        0.0
    } else {
        v
    }
}

/// The chroma-key matte (before invert) for a distance `d` (ADR-C007 §3):
/// [`luma_matte`] on the distance with a subnormal clip or gain flushed to 0
/// (a GPU may flush them). Total:
/// a NaN or negative gain, a NaN clip or a NaN distance gives 0; a pixel at
/// the reference (`d = 0`) with `clip >= 0` gives 0 (keyed out).
#[must_use]
pub fn chroma_matte(d: f32, clip: f32, gain: f32) -> f32 {
    luma_matte(d, flush_subnormal(clip), flush_subnormal(gain))
}

/// Euclidean distance between two linear RGB triples (f32, in the order the
/// GPU kernel evaluates it).
#[must_use]
pub fn chroma_distance(pixel: [f32; 3], reference: [f32; 3]) -> f32 {
    let dr = pixel[0] - reference[0];
    let dg = pixel[1] - reference[1];
    let db = pixel[2] - reference[2];
    (dr * dr + dg * dg + db * db).sqrt()
}

/// The chroma reference as the fill's own 8-bit `(Y, Cb, Cr)` codes: the
/// operator's full-range R'G'B' codes (`code / 255`) through the fill's
/// matrix ([`crate::matrix::rgb_to_yuv`]) and range compress. Quantizing the
/// reference onto the fill's code grid is what makes "the reference colour"
/// exact (review K7 MEDIA-1): a fill pixel carrying these codes runs the
/// identical front half as the reference and is at distance exactly 0.
///
/// # Errors
///
/// [`crate::Error::UnresolvedColor`] / `UnsupportedMatrix` for a fill matrix
/// or range the pipeline cannot decode (exactly the fill's own error).
pub fn chroma_reference_codes(
    reference: [u8; 3],
    color: multiview_core::color::ColorInfo,
) -> Result<(u8, u8, u8)> {
    let r = crate::range::require_resolved(color.range)?;
    let g = reference.map(|c| f32::from(c) / 255.0);
    let yuv = crate::matrix::rgb_to_yuv(g[0], g[1], g[2], color.matrix)?;
    Ok((
        crate::range::compress_luma(yuv[0], r),
        crate::range::compress_chroma(yuv[1], r),
        crate::range::compress_chroma(yuv[2], r),
    ))
}

/// The chroma reference in the fill's own-gamut linear RGB: its codes
/// ([`chroma_reference_codes`]) through **the same** front half (steps 1-3)
/// as the fill's pixels, with `luts` exactly when the pixels use them, so
/// equal codes give a distance of exactly 0 whatever the transfer
/// evaluation (review K7 MEDIA-1). Computed once per frame per keyed tile.
///
/// # Errors
///
/// The fill's own colour errors (unresolved or unsupported axis).
pub fn chroma_reference_linear(
    reference: [u8; 3],
    color: multiview_core::color::ColorInfo,
    luts: Option<&crate::transfer_lut::LutSet>,
) -> Result<[f32; 3]> {
    let (y8, cb8, cr8) = chroma_reference_codes(reference, color)?;
    match luts {
        Some(lut) => lut.tile_yuv_to_tile_linear(y8, cb8, cr8, color),
        None => crate::pipeline::tile_yuv_to_tile_linear(y8, cb8, cr8, color),
    }
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
/// source is sampled at its own resolution. `chroma` is the fill pixel's
/// tile-gamut linear RGB and the reference's (both used only by a chroma
/// key; `None` otherwise).
///
/// # Errors
///
/// [`crate::Error::UnresolvedColor`] when the key source's range is
/// unresolved.
#[allow(clippy::too_many_arguments)]
// reason: one pixel's sampling context (fill coords, destination offset and
// size, fill luma and linear colour); a struct would be built per pixel for
// no gain.
pub(crate) fn key_factor(
    key: &TileKey<'_>,
    sx: u32,
    sy: u32,
    ddx: u32,
    ddy: u32,
    dst_w: u32,
    dst_h: u32,
    fill_y: f32,
    chroma: Option<([f32; 3], [f32; 3])>,
) -> Result<f32> {
    if let Some(mask) = key.mask {
        if !mask.contains(sx, sy) {
            return Ok(0.0);
        }
    }
    if let KeyKind::Chroma { clip, gain, .. } = key.kind {
        // The fill's own colour; a key source is ignored. A caller without
        // the colour (never the compositor) reads as fully keyed out.
        let matte = chroma.map_or(0.0, |(px, reference)| {
            chroma_matte(chroma_distance(px, reference), clip, gain)
        });
        return Ok(if key.invert { 1.0 - matte } else { matte });
    }
    let signal = match key.signal_source() {
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
        // Handled above.
        KeyKind::Chroma { .. } => 0.0,
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
            let m = SceneMerge::Mix { p }.apply(x, x, (0, 0), (1, 1));
            assert!((m.r - x.r).abs() < 1e-6 && (m.a - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn nan_weights_are_treated_as_zero() {
        assert_eq!(unit(f32::NAN), 0.0);
        assert_eq!(unit(-1.0), 0.0);
        assert_eq!(unit(2.0), 1.0);
    }

    // ---- Chroma key (K7, ADR-C007 §3) ----

    use multiview_core::color::{
        ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
    };

    const BT709_LIMITED: ColorInfo = ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    };

    fn chroma_at(key: &TileKey<'_>, px: [f32; 3], reference: [f32; 3]) -> f32 {
        key_factor(key, 0, 0, 0, 0, 1, 1, 0.5, Some((px, reference))).unwrap_or(f32::NAN)
    }

    #[test]
    fn the_exact_reference_colour_is_keyed_out() {
        let green = chroma_reference_linear([0, 177, 64], BT709_LIMITED, None).unwrap();
        for (clip, gain) in [(0.0, 1.0), (0.1, 4.0), (0.0, f32::INFINITY), (0.3, 1e30)] {
            let key = TileKey::chroma([0, 177, 64], clip, gain);
            assert_eq!(
                chroma_at(&key, green, green),
                0.0,
                "clip {clip} gain {gain}"
            );
            // Inverted: the reference is the only thing kept.
            assert_eq!(chroma_at(&key.inverted(true), green, green), 1.0);
        }
    }

    #[test]
    fn a_far_colour_is_kept() {
        let green = chroma_reference_linear([0, 177, 64], BT709_LIMITED, None).unwrap();
        let magenta = chroma_reference_linear([255, 0, 255], BT709_LIMITED, None).unwrap();
        let key = TileKey::chroma([0, 177, 64], 0.1, 4.0);
        assert_eq!(chroma_at(&key, magenta, green), 1.0);
        assert_eq!(chroma_at(&key.inverted(true), magenta, green), 0.0);
    }

    #[test]
    fn the_soft_edge_is_monotonic_and_bounded_in_distance() {
        let reference = [0.0, 0.4, 0.05];
        let key = TileKey::chroma([0, 0, 0], 0.05, 5.0);
        let mut last = 0.0_f32;
        for i in 0..=400_u16 {
            let t = f32::from(i) / 400.0;
            // Walk away from the reference along a fixed direction.
            let px = [t * 0.6, 0.4 - t * 0.3, 0.05 + t * 0.5];
            let m = chroma_at(&key, px, reference);
            assert!((0.0..=1.0).contains(&m), "{m}");
            assert!(m >= last, "not monotonic at {t}: {m} < {last}");
            last = m;
        }
        assert_eq!(last, 1.0);
        // Inside the clip the pixel is fully keyed out; the edge is a ramp.
        assert_eq!(chroma_matte(0.05, 0.05, 5.0), 0.0);
        assert!((chroma_matte(0.15, 0.05, 5.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn degenerate_chroma_parameters_are_total() {
        let nan = f32::NAN;
        let inf = f32::INFINITY;
        let sub = 1e-40_f32;
        for clip in [nan, inf, -inf, sub, -sub, 0.0, 0.2, -0.5, f32::MAX] {
            for gain in [nan, inf, -inf, -1.0, 0.0, 1.0, 1e30, f32::MAX] {
                for d in [0.0, sub, 0.1, 0.2, 1.0, 1e10, inf] {
                    let m = chroma_matte(d, clip, gain);
                    assert!(
                        (0.0..=1.0).contains(&m),
                        "d={d} clip={clip} gain={gain}: {m}"
                    );
                }
            }
        }
        // A NaN clip or gain never keeps anything; a NaN distance neither.
        assert_eq!(chroma_matte(1.0, nan, 1.0), 0.0);
        assert_eq!(chroma_matte(1.0, 0.0, nan), 0.0);
        assert_eq!(chroma_matte(nan, 0.0, 1.0), 0.0);
        // A subnormal clip is 0: at the reference the matte is 0, not a
        // flushed-vs-unflushed difference between kernels.
        assert_eq!(chroma_matte(0.0, -sub, inf), 0.0);
        // An infinite gain is a strict step at the clip.
        assert_eq!(chroma_matte(0.2, 0.2, inf), 0.0);
        assert_eq!(chroma_matte(0.200_001, 0.2, inf), 1.0);
        // A key without the colour context reads as keyed out, never NaN.
        let key = TileKey::chroma([0, 255, 0], 0.0, 1.0);
        assert_eq!(key_factor(&key, 0, 0, 0, 0, 1, 1, 0.5, None).unwrap(), 0.0);
    }

    #[test]
    fn a_chroma_key_ignores_a_key_source_and_honours_the_mask() {
        let color = multiview_core::color::ColorInfo {
            primaries: multiview_core::color::ColorPrimaries::Bt709,
            transfer: TransferCharacteristic::Bt709,
            matrix: multiview_core::color::MatrixCoefficients::Bt709,
            range: multiview_core::color::ColorRange::Limited,
        };
        let black = Nv12Image::solid(4, 4, 16, 128, 128, color).unwrap();
        let key = TileKey::chroma([0, 255, 0], 0.0, 1.0).with_source(&black);
        assert!(key.signal_source().is_none());
        let far = ([1.0, 0.0, 1.0], [0.0, 1.0, 0.0]);
        assert_eq!(
            key_factor(&key, 0, 0, 0, 0, 4, 4, 0.0, Some(far)).unwrap(),
            1.0
        );
        let masked = key.masked(KeyMask {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
        });
        assert_eq!(
            key_factor(&masked, 0, 0, 0, 0, 4, 4, 0.0, Some(far)).unwrap(),
            0.0
        );
        assert_eq!(
            key_factor(&masked, 1, 1, 0, 0, 4, 4, 0.0, Some(far)).unwrap(),
            1.0
        );
    }

    #[test]
    fn the_reference_is_quantized_to_the_fill_codes_and_decoded_like_a_pixel() {
        // White and black land on the limited-range code extremes.
        assert_eq!(
            chroma_reference_codes([255, 255, 255], BT709_LIMITED).unwrap(),
            (235, 128, 128)
        );
        assert_eq!(
            chroma_reference_codes([0; 3], BT709_LIMITED).unwrap(),
            (16, 128, 128)
        );
        let full = ColorInfo {
            range: ColorRange::Full,
            ..BT709_LIMITED
        };
        assert_eq!(
            chroma_reference_codes([128; 3], full).unwrap(),
            (128, 128, 128)
        );
        // The linear reference is exactly the front half of those codes.
        let (y, cb, cr) = chroma_reference_codes([0, 177, 64], BT709_LIMITED).unwrap();
        let want = crate::pipeline::tile_yuv_to_tile_linear(y, cb, cr, BT709_LIMITED).unwrap();
        let got = chroma_reference_linear([0, 177, 64], BT709_LIMITED, None).unwrap();
        assert_eq!(got.map(f32::to_bits), want.map(f32::to_bits));
        let unresolved = ColorInfo {
            transfer: TransferCharacteristic::Unspecified,
            ..BT709_LIMITED
        };
        assert!(chroma_reference_linear([0; 3], unresolved, None).is_err());
    }
}
