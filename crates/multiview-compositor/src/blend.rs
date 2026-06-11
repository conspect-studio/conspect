//! Premultiplied-alpha source-over compositing in **linear light**.
//!
//! Step 5 of the fixed pipeline (invariant #8 / ADR-C003): scaling and alpha
//! blending happen on **linear** RGB with **premultiplied** alpha. Compositing
//! in gamma/YUV space causes dark fringing on edges and overlays and wrong
//! mids, independent of every other color-correctness concern.
//!
//! A [`LinearRgba`] carries straight (non-premultiplied) linear RGB plus alpha;
//! [`LinearRgba::premultiplied`] yields the premultiplied form, and
//! [`over`] composites a premultiplied `src` over a premultiplied `dst` with
//! the Porter-Duff source-over operator `out = src + dst * (1 - src.a)`.

/// A linear-light RGBA color (straight alpha) in the canvas working space.
///
/// Channels are linear (post-EOTF) `f32`; alpha is in `[0, 1]`. This is the
/// representation tiles converge to before blending.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearRgba {
    /// Linear red.
    pub r: f32,
    /// Linear green.
    pub g: f32,
    /// Linear blue.
    pub b: f32,
    /// Straight (non-premultiplied) alpha in `[0, 1]`.
    pub a: f32,
}

impl LinearRgba {
    /// An opaque linear color from RGB (alpha `1.0`).
    #[must_use]
    pub const fn opaque(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    /// A fully transparent color (all channels `0.0`); the identity for
    /// [`over`] as a destination.
    pub const TRANSPARENT: Self = Self {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    /// The premultiplied form `(r*a, g*a, b*a, a)`.
    ///
    /// Premultiplication is what makes linear src-over correct at partially
    /// transparent edges (ADR-C003).
    #[must_use]
    pub fn premultiplied(self) -> PremulRgba {
        PremulRgba {
            r: self.r * self.a,
            g: self.g * self.a,
            b: self.b * self.a,
            a: self.a,
        }
    }
}

/// A premultiplied linear-light RGBA color: each color channel already carries
/// its alpha factor (`channel = straight_channel * a`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PremulRgba {
    /// Premultiplied linear red (`r * a`).
    pub r: f32,
    /// Premultiplied linear green (`g * a`).
    pub g: f32,
    /// Premultiplied linear blue (`b * a`).
    pub b: f32,
    /// Alpha in `[0, 1]`.
    pub a: f32,
}

impl PremulRgba {
    /// A fully transparent premultiplied color (the [`over`] destination
    /// identity).
    pub const TRANSPARENT: Self = Self {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    /// Recover the straight-alpha [`LinearRgba`] by un-premultiplying.
    ///
    /// A zero alpha yields a fully transparent color with zero RGB (the only
    /// well-defined un-premultiplication of `a == 0`).
    #[must_use]
    pub fn unpremultiplied(self) -> LinearRgba {
        if self.a == 0.0 {
            LinearRgba::TRANSPARENT
        } else {
            LinearRgba {
                r: self.r / self.a,
                g: self.g / self.a,
                b: self.b / self.a,
                a: self.a,
            }
        }
    }
}

/// Porter-Duff **source-over** of premultiplied `src` over premultiplied `dst`,
/// in linear light: `out = src + dst * (1 - src.a)`, applied per channel
/// (including alpha).
///
/// Both operands and the result are premultiplied. Compositing a stack of tiles
/// folds them back-to-front with this operator.
#[must_use]
pub fn over(src: PremulRgba, dst: PremulRgba) -> PremulRgba {
    let inv = 1.0 - src.a;
    PremulRgba {
        r: dst.r.mul_add(inv, src.r),
        g: dst.g.mul_add(inv, src.g),
        b: dst.b.mul_add(inv, src.b),
        a: dst.a.mul_add(inv, src.a),
    }
}
