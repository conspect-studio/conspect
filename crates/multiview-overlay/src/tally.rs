//! Multi-region tally **border** render model (broadcast brief §2).
//!
//! A multiviewer tile carries tally in up to three independently-driven
//! regions, mirroring the TSL v4.0/v5.0 per-element model (LH / Text / RH, each
//! `0 = off / 1 = red / 2 = green / 3 = amber`):
//!
//! * [`TallyRegion::Left`] — a vertical strip down the **left** edge.
//! * [`TallyRegion::Right`] — a vertical strip down the **right** edge.
//! * [`TallyRegion::Text`] — a horizontal band across the **bottom** of the tile
//!   (the Under-Monitor Display band the [`crate::umd`] label sits in), whose
//!   colour is the "text" tally.
//!
//! Each region is driven by a resolved [`multiview_core::tally::TallyState`] from
//! the tally arbiter (in `multiview-engine`). This module is **pure geometry +
//! colour**: it turns a tile [`PixelRect`] and the per-region states into the
//! drawable strips, their premultiplied-RGBA fill, and — critically for
//! accessibility — a **text label** for each lit region so the tally state is
//! conveyed by border *and* text, never colour alone.
//!
//! Nothing here touches the GPU, a rasterizer, or a live input frame: the tally
//! border is drawable the instant the arbiter resolves, independent of any
//! decoded frame (overlays are input-decoupled, ADR-R008).

use multiview_core::tally::{Brightness, BusSource, TallyColor, TallyState};
use serde::{Deserialize, Serialize};

use crate::geometry::PixelRect;

/// Which tally element a region renders, mirroring the TSL per-element model.
///
/// The discriminant order (`Left < Right < Text`) is the stable iteration order
/// of [`TallyModel::regions`]. Serialised tagged (`snake_case` variant names);
/// never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TallyRegion {
    /// The left-hand (`LH`) vertical tally strip.
    Left,
    /// The right-hand (`RH`) vertical tally strip.
    Right,
    /// The bottom text-band (`Text`) tally, behind the UMD label.
    Text,
}

impl TallyRegion {
    /// The three regions in their canonical iteration order.
    const ALL: [Self; 3] = [Self::Left, Self::Right, Self::Text];
}

/// Cosmetic styling for the tally border. Geometry is derived from these
/// thicknesses; colour is derived from each region's [`TallyState`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TallyStyle {
    /// Thickness, in pixels, of the left/right vertical tally strips.
    pub border_px: f32,
    /// Height, in pixels, of the bottom text-band tally.
    pub text_band_px: f32,
}

impl Default for TallyStyle {
    fn default() -> Self {
        Self {
            border_px: 6.0,
            text_band_px: 18.0,
        }
    }
}

/// One resolved, lit tally region: which [`TallyRegion`] it is, the pixel strip
/// to fill, its premultiplied-RGBA [`fill`](ResolvedRegion::fill), and an
/// accessibility text [`label`](ResolvedRegion::label).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRegion {
    /// Which region this is.
    pub region: TallyRegion,
    /// The pixel strip to fill on the tile.
    pub rect: PixelRect,
    /// Premultiplied-RGBA fill (`0.0..=1.0`), hue from the tally colour and
    /// magnitude scaled by brightness.
    pub fill: [f32; 4],
    /// A text label (e.g. `"red · program"`) so the state reads without relying
    /// on colour alone (accessibility).
    pub label: String,
}

/// The builder/config for a tile's tally border: a [`TallyStyle`] plus a
/// per-region [`TallyState`]. Resolve it against a tile [`PixelRect`] with
/// [`TallyBorder::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TallyBorder {
    style: TallyStyle,
    left: Option<TallyState>,
    right: Option<TallyState>,
    text: Option<TallyState>,
}

impl TallyBorder {
    /// A tally border with the given style and no regions set (nothing drawn).
    #[must_use]
    pub const fn new(style: TallyStyle) -> Self {
        Self {
            style,
            left: None,
            right: None,
            text: None,
        }
    }

    /// Set a region's tally state, builder-style.
    #[must_use]
    pub const fn with_region(mut self, region: TallyRegion, state: TallyState) -> Self {
        match region {
            TallyRegion::Left => self.left = Some(state),
            TallyRegion::Right => self.right = Some(state),
            TallyRegion::Text => self.text = Some(state),
        }
        self
    }

    /// The configured state for a region, if any.
    #[must_use]
    pub const fn state_for(&self, region: TallyRegion) -> Option<TallyState> {
        match region {
            TallyRegion::Left => self.left,
            TallyRegion::Right => self.right,
            TallyRegion::Text => self.text,
        }
    }

    /// Resolve every **lit** region into a drawable [`TallyModel`] for `tile`.
    ///
    /// A region with an [unlit](multiview_core::tally::TallyState::is_lit) state (or
    /// no configured state) draws nothing and is omitted. The left/right strips
    /// stop above the text band so the regions never overpaint each other.
    #[must_use]
    pub fn resolve(&self, tile: PixelRect) -> TallyModel {
        let border = self.style.border_px.max(0.0);
        let band = self.style.text_band_px.max(0.0);
        // The text band is only reserved at the bottom when the text region is
        // lit; otherwise the side strips may run the full height.
        let band_reserved = if self.is_lit(TallyRegion::Text) {
            band.min(tile.height)
        } else {
            0.0
        };
        let strip_height = (tile.height - band_reserved).max(0.0);

        let mut regions = Vec::new();
        for region in TallyRegion::ALL {
            let Some(state) = self.state_for(region) else {
                continue;
            };
            if !state.is_lit() {
                continue;
            }
            let rect = match region {
                TallyRegion::Left => PixelRect {
                    x: tile.x,
                    y: tile.y,
                    width: border.min(tile.width),
                    height: strip_height,
                },
                TallyRegion::Right => PixelRect {
                    x: tile.right() - border.min(tile.width),
                    y: tile.y,
                    width: border.min(tile.width),
                    height: strip_height,
                },
                TallyRegion::Text => PixelRect {
                    x: tile.x,
                    y: tile.bottom() - band_reserved,
                    width: tile.width,
                    height: band_reserved,
                },
            };
            regions.push(ResolvedRegion {
                region,
                rect,
                fill: fill_for(state),
                label: label_for(state),
            });
        }
        TallyModel { regions }
    }

    fn is_lit(&self, region: TallyRegion) -> bool {
        self.state_for(region).is_some_and(TallyState::is_lit)
    }
}

/// The resolved, drawable tally border for one tile: the lit regions in stable
/// [`TallyRegion`] order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TallyModel {
    regions: Vec<ResolvedRegion>,
}

impl TallyModel {
    /// The lit regions, in canonical (`Left`, `Right`, `Text`) order.
    #[must_use]
    pub fn regions(&self) -> &[ResolvedRegion] {
        &self.regions
    }

    /// The resolved strip for `region`, if it is lit.
    #[must_use]
    pub fn rect_for(&self, region: TallyRegion) -> Option<PixelRect> {
        self.find(region).map(|r| r.rect)
    }

    /// The premultiplied-RGBA fill for `region`, if it is lit.
    #[must_use]
    pub fn color_for(&self, region: TallyRegion) -> Option<[f32; 4]> {
        self.find(region).map(|r| r.fill)
    }

    /// The accessibility text label for `region`, if it is lit.
    #[must_use]
    pub fn label_for(&self, region: TallyRegion) -> Option<&str> {
        self.find(region).map(|r| r.label.as_str())
    }

    fn find(&self, region: TallyRegion) -> Option<&ResolvedRegion> {
        self.regions.iter().find(|r| r.region == region)
    }
}

/// The base (full-brightness) premultiplied-RGB hue for a tally colour. The
/// alpha is opaque; brightness scales all four channels in [`fill_for`].
fn hue(color: TallyColor) -> [f32; 3] {
    match color {
        // Off has no hue; callers never reach this for an unlit region.
        TallyColor::Off => [0.0, 0.0, 0.0],
        TallyColor::Red => [1.0, 0.0, 0.0],
        TallyColor::Green => [0.0, 1.0, 0.0],
        // Amber: full red, ~0.75 green, no blue.
        TallyColor::Amber => [1.0, 0.749, 0.0],
        // A future palette colour (`TallyColor` is `#[non_exhaustive]`): fall
        // back to a neutral white so an unknown lit lamp is still visible, and
        // rely on the text label to convey its meaning.
        _ => [1.0, 1.0, 1.0],
    }
}

/// Map a 2-bit [`Brightness`] level (`0..=3`) to a `0.0..=1.0` scale. Level 0
/// would be dark; lit regions are level `>= 1`, so the dimmest lit step is a
/// visible fraction and full is `1.0`.
fn brightness_scale(b: Brightness) -> f32 {
    // 0->0.0, 1->1/3, 2->2/3, 3->1.0. Exact for the four levels.
    f32::from(b.level()) / 3.0
}

/// The premultiplied-RGBA fill for a lit tally state: the colour hue scaled by
/// brightness (premultiplied, so each RGB channel is already multiplied by the
/// opaque alpha of `1.0`).
fn fill_for(state: TallyState) -> [f32; 4] {
    let [r, g, b] = hue(state.color);
    let s = brightness_scale(state.brightness);
    [r * s, g * s, b * s, 1.0]
}

/// A short accessibility label naming the tally colour and its source bus, so
/// the state reads as text (not colour alone).
fn label_for(state: TallyState) -> String {
    format!("{} · {}", color_word(state.color), bus_word(state.source))
}

/// The lower-case word for a tally colour.
const fn color_word(color: TallyColor) -> &'static str {
    match color {
        TallyColor::Off => "off",
        TallyColor::Red => "red",
        TallyColor::Green => "green",
        TallyColor::Amber => "amber",
        // A future palette colour (`TallyColor` is `#[non_exhaustive]`).
        _ => "tally",
    }
}

/// A short human label for a bus source.
fn bus_word(source: BusSource) -> String {
    match source {
        BusSource::Program => "program".to_owned(),
        BusSource::Preview => "preview".to_owned(),
        BusSource::Aux { index } => format!("aux {index}"),
        BusSource::Iso { index } => format!("iso {index}"),
        // A future bus kind (`BusSource` is `#[non_exhaustive]`).
        _ => "bus".to_owned(),
    }
}
