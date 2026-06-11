//! Safe-area graticules per SMPTE ST 2046-1.
//!
//! ST 2046-1 ("Format and Aspect Ratio — Production Aperture, Clean Aperture
//! and Safe Areas") defines, relative to the **Production Aperture** (the full
//! coded raster Multiview composites into), two centred safe-area rectangles:
//!
//! * **Action-safe** — 93 % of the Production Aperture. Picture content the
//!   audience is essentially guaranteed to see.
//! * **Title-safe** — 90 % of the Production Aperture. The region graphics and
//!   readable text must stay within.
//!
//! (The alternate-aspect protection of RP 2046-2 is 90/90 and the legacy RP 218
//! graticule predates ST 2046-1; this module implements the current ST 2046-1
//! figures. The figures are cited directly from ST 2046-1.)
//!
//! A **center cross** marks the geometric centre of the raster.
//!
//! Everything here is **pure geometry**: enabled markers resolve to exact
//! pixel rectangles at a given [`CanvasSize`], independent of any input frame.
//! Each resolved rectangle carries its [`SafeAreaKind`] so the renderer can
//! draw a text/glyph label — the marker's meaning is conveyed beyond colour
//! alone (accessibility).

use serde::{Deserialize, Serialize};

use crate::geometry::PixelRect;
use crate::resolve::CanvasSize;

/// A safe-area graticule defined by ST 2046-1 as a centred fraction of the
/// Production Aperture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SafeAreaKind {
    /// Action-safe: **93 %** of the Production Aperture (ST 2046-1).
    ActionSafe,
    /// Title-safe: **90 %** of the Production Aperture (ST 2046-1).
    TitleSafe,
}

impl SafeAreaKind {
    /// The centred fraction of the Production Aperture this graticule covers, as
    /// specified by SMPTE ST 2046-1 (0.93 for action-safe, 0.90 for title-safe).
    #[must_use]
    pub const fn fraction(self) -> f32 {
        match self {
            Self::ActionSafe => 0.93,
            Self::TitleSafe => 0.90,
        }
    }

    /// A short, descriptive label for the graticule.
    ///
    /// Used so the renderer can convey the marker's meaning as text/glyph rather
    /// than relying on colour alone (accessibility).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ActionSafe => "action-safe",
            Self::TitleSafe => "title-safe",
        }
    }

    /// The centred pixel rectangle for this graticule on `canvas`.
    ///
    /// The rectangle is [`fraction`](Self::fraction) of the canvas on each axis,
    /// centred (equal inset on opposing edges), matching ST 2046-1's
    /// centred-of-the-Production-Aperture definition.
    #[must_use]
    pub fn rect(self, canvas: CanvasSize) -> PixelRect {
        centred_fraction(canvas, self.fraction())
    }
}

/// A safe-area rectangle resolved to pixels, tagged with the [`SafeAreaKind`] it
/// came from so the renderer can label it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SafeAreaRect {
    /// Which graticule this rectangle represents.
    pub kind: SafeAreaKind,
    /// The centred pixel rectangle.
    pub rect: PixelRect,
}

/// A center-cross marker at the geometric centre of the raster, with the
/// half-length of each arm in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CenterCross {
    /// Centre x in pixels (canvas width / 2).
    pub x: f32,
    /// Centre y in pixels (canvas height / 2).
    pub y: f32,
    /// Half-length of each cross arm, in pixels.
    pub arm_px: f32,
}

/// Cosmetic styling for the safe-area graticules. Geometry is independent of
/// style; this only affects how the renderer strokes the markers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SafeAreaStyle {
    /// Stroke width in pixels.
    pub stroke_px: f32,
    /// Premultiplied-RGBA stroke colour (`0.0..=1.0`); the [`SafeAreaKind`]
    /// label is still drawn so meaning does not depend on this colour.
    pub color: [f32; 4],
}

impl Default for SafeAreaStyle {
    fn default() -> Self {
        Self {
            stroke_px: 2.0,
            // Neutral white: meaning is conveyed by the text/glyph label, not
            // this colour (accessibility).
            color: [1.0, 1.0, 1.0, 0.75],
        }
    }
}

/// Which safe-area markers to draw. Pure configuration; resolve against a
/// [`CanvasSize`] with [`SafeAreaMarkers::resolve`].
///
/// Default is **all markers off** — nothing is drawn until explicitly enabled.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SafeAreaMarkers {
    /// Draw the action-safe (93 %) graticule.
    pub action_safe: bool,
    /// Draw the title-safe (90 %) graticule.
    pub title_safe: bool,
    /// Draw the center cross.
    pub center_cross: bool,
    /// Cosmetic styling.
    pub style: SafeAreaStyle,
}

impl SafeAreaMarkers {
    /// Enable or disable a [`SafeAreaKind`] graticule, builder-style.
    #[must_use]
    pub const fn with_kind(mut self, kind: SafeAreaKind, on: bool) -> Self {
        match kind {
            SafeAreaKind::ActionSafe => self.action_safe = on,
            SafeAreaKind::TitleSafe => self.title_safe = on,
        }
        self
    }

    /// Enable or disable the center cross, builder-style.
    #[must_use]
    pub const fn with_center_cross(mut self, on: bool) -> Self {
        self.center_cross = on;
        self
    }

    /// Resolve the enabled markers into a drawable [`SafeAreaModel`] at `canvas`.
    #[must_use]
    pub fn resolve(self, canvas: CanvasSize) -> SafeAreaModel {
        let mut rects = Vec::new();
        if self.action_safe {
            rects.push(SafeAreaRect {
                kind: SafeAreaKind::ActionSafe,
                rect: SafeAreaKind::ActionSafe.rect(canvas),
            });
        }
        if self.title_safe {
            rects.push(SafeAreaRect {
                kind: SafeAreaKind::TitleSafe,
                rect: SafeAreaKind::TitleSafe.rect(canvas),
            });
        }
        let center_cross = self.center_cross.then(|| {
            let (cw, ch) = (
                crate::geometry::f32_from_u32(canvas.width),
                crate::geometry::f32_from_u32(canvas.height),
            );
            // Arm length: a small fixed fraction of the shorter axis.
            let arm_px = cw.min(ch) * 0.02;
            CenterCross {
                x: cw / 2.0,
                y: ch / 2.0,
                arm_px,
            }
        });
        SafeAreaModel {
            rects,
            center_cross,
            style: self.style,
        }
    }
}

/// The resolved, drawable safe-area model for one canvas: the enabled graticule
/// rectangles (each tagged with its kind) and an optional center cross.
#[derive(Debug, Clone, PartialEq)]
pub struct SafeAreaModel {
    /// The enabled graticule rectangles, each tagged with its [`SafeAreaKind`].
    pub rects: Vec<SafeAreaRect>,
    /// The center cross, if enabled.
    pub center_cross: Option<CenterCross>,
    /// Cosmetic styling carried through to the renderer.
    pub style: SafeAreaStyle,
}

/// A rectangle that is `fraction` of `canvas` on each axis, centred (equal inset
/// on opposing edges).
fn centred_fraction(canvas: CanvasSize, fraction: f32) -> PixelRect {
    let cw = crate::geometry::f32_from_u32(canvas.width);
    let ch = crate::geometry::f32_from_u32(canvas.height);
    let width = cw * fraction;
    let height = ch * fraction;
    PixelRect {
        x: (cw - width) / 2.0,
        y: (ch - height) / 2.0,
        width,
        height,
    }
}
