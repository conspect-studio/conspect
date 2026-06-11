//! The serializable, ordered overlay layer stack (ADR-R008 / resilience-and-av
//! §7).
//!
//! An [`OverlayStack`] holds [`OverlayLayer`]s; the compositor walks them
//! back-to-front (ascending `z`, insertion-order-stable for equal `z`) and
//! blends premultiplied "over". Each layer carries *what* to draw
//! ([`LayerKind`] + its style), *where* (a [`Target`] surface plus an anchored
//! [`Placement`]), and *how* (`z`, `opacity`, [`BlendMode`], `visible`).
//!
//! This module is the pure model only — no rasterization. The actual glyph /
//! libass / SDF rendering (ADR-R007/R008) lives behind the off-by-default
//! `libass` feature and in the compositor crate; here we describe layers and
//! resolve them into the backend-agnostic draw list in [`crate::resolve`].

use serde::{Deserialize, Serialize};

use crate::alert::AlertCard;
use crate::geometry::{Anchor, BoxSize, NormRect, Padding};

/// How a layer's RGBA source is composited over what is already on the canvas.
///
/// Premultiplied-alpha "over" is the default and the only correct general
/// blend for antialiased overlays (ADR-R008: a straight/premultiplied mismatch
/// halos every edge). The others are opt-in effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BlendMode {
    /// Premultiplied source-over (`src=One`, `dst=OneMinusSrcAlpha`).
    #[default]
    Over,
    /// Additive (`src=One`, `dst=One`) — glows, highlights.
    Add,
    /// Source replaces destination (no blend); for opaque fills.
    Replace,
}

/// Which surface a layer is positioned against.
///
/// Adjacently tagged so the serialized form carries a `surface` discriminant
/// (never `untagged`, per the workspace serde policy).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "surface", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Target {
    /// The whole output canvas.
    #[default]
    FullCanvas,
    /// A specific tile, given by its normalized rectangle on the canvas. The
    /// rectangle matches the bound [`multiview_core::layout::Cell`] so a per-tile
    /// overlay rides along under live layout changes (re-bound atomically with
    /// the tile, ADR-R008).
    Tile {
        /// The tile's normalized rectangle on the canvas.
        rect: NormRect,
    },
}

/// Where, within its [`Target`], an overlay box sits: a normalized sub-region of
/// the target, an [`Anchor`] inside that region, edge [`Padding`], and the box
/// [`BoxSize`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// Normalized sub-region of the target the box is anchored within.
    pub region: NormRect,
    /// Where the box pins inside `region`.
    pub anchor: Anchor,
    /// Edge insets between the region and the box.
    pub padding: Padding,
    /// The box's pixel size.
    pub size: BoxSize,
}

impl Default for Placement {
    fn default() -> Self {
        Self {
            region: NormRect::FULL,
            anchor: Anchor::default(),
            padding: Padding::default(),
            size: BoxSize::default(),
        }
    }
}

/// Style for a text label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextStyle {
    /// The string to render.
    pub text: String,
    /// Point size (logical pixels at the canvas resolution).
    pub size_px: f32,
    /// RGBA fill color, premultiplied at upload time (ADR-R008), `0.0..=1.0`.
    pub color: [f32; 4],
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            text: String::new(),
            size_px: 32.0,
            color: [1.0, 1.0, 1.0, 1.0],
        }
    }
}

/// Whether a clock shows wall-clock time-of-day, an analog face, or a running
/// program-time counter (the always-ticking element of the soak gate,
/// resilience-and-av §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ClockKind {
    /// Digital time-of-day.
    #[default]
    Digital,
    /// Analog face (hour/minute/second hands).
    Analog,
    /// Running program/output time counter.
    ProgramTimecode,
}

/// Style for a clock overlay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockStyle {
    /// Which clock presentation to render.
    pub kind: ClockKind,
    /// `strftime`-style format for the [`ClockKind::Digital`] presentation.
    pub format: String,
    /// Whether to show seconds (drives the per-second dirty-region upload).
    pub show_seconds: bool,
}

impl Default for ClockStyle {
    fn default() -> Self {
        Self {
            kind: ClockKind::Digital,
            format: "%H:%M:%S".to_owned(),
            show_seconds: true,
        }
    }
}

/// Orientation of an audio meter's bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MeterOrientation {
    /// Bars grow upward (typical PPM/VU column).
    #[default]
    Vertical,
    /// Bars grow rightward.
    Horizontal,
}

/// Style for an audio meter overlay. Levels are pushed as small uniforms each
/// frame (meters-as-geometry, ADR-R008); this is the static styling only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeterStyle {
    /// Number of channels/bars to display.
    pub channels: u8,
    /// Bar orientation.
    pub orientation: MeterOrientation,
    /// Whether to draw a peak-hold marker.
    pub peak_hold: bool,
}

impl Default for MeterStyle {
    fn default() -> Self {
        Self {
            channels: 2,
            orientation: MeterOrientation::Vertical,
            peak_hold: true,
        }
    }
}

/// The kind of an overlay layer and its kind-specific style/state.
///
/// Internally tagged on `kind` so the serialized union carries a discriminant
/// (never `untagged`, per the workspace serde policy). The kinds match
/// resilience-and-av §7: `text | meter | logo | lower_third | clock |
/// alert_card | subtitle`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LayerKind {
    /// A text label.
    Text(TextStyle),
    /// A wall-clock / analog / program-timecode clock.
    Clock(ClockStyle),
    /// An audio level meter.
    Meter(MeterStyle),
    /// An alert card with its own state machine ([`AlertCard`]).
    AlertCard(AlertCard),
    /// A static logo/bug (image asset bound elsewhere).
    Logo,
    /// A lower-third graphic.
    LowerThird,
    /// A burned-in subtitle layer (rasterized via the off-by-default `libass`
    /// feature; the model here is the placement/visibility only).
    Subtitle,
}

/// One overlay layer in the stack: the full descriptor the compositor consumes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayLayer {
    /// Stable, unique-within-the-stack identifier (addressable by the API).
    pub id: String,
    /// What this layer draws, with its style.
    pub kind: LayerKind,
    /// Which surface the layer is positioned against.
    pub target: Target,
    /// Where, within the target, the box sits.
    pub placement: Placement,
    /// Stacking order: higher draws on top (resolved back-to-front).
    pub z: i32,
    /// Layer opacity multiplier in `0.0..=1.0` (clamped at resolve time).
    pub opacity: f32,
    /// How the layer blends onto the canvas.
    pub blend: BlendMode,
    /// Whether the layer is drawn this frame.
    pub visible: bool,
}

/// An ordered, serializable stack of overlay layers.
///
/// Layers are stored in insertion order; [`OverlayStack::draw_order`] yields
/// them sorted by ascending `z` with stable ties (back-to-front), which is the
/// order the compositor blends them in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OverlayStack {
    layers: Vec<OverlayLayer>,
}

impl OverlayStack {
    /// An empty stack.
    #[must_use]
    pub const fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Append a layer (kept in insertion order; equal-`z` ties resolve stably by
    /// insertion order).
    pub fn push(&mut self, layer: OverlayLayer) {
        self.layers.push(layer);
    }

    /// The layers in their declared (insertion) order.
    #[must_use]
    pub fn layers(&self) -> &[OverlayLayer] {
        &self.layers
    }

    /// Number of layers in the stack.
    #[must_use]
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    /// Whether the stack has no layers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Iterate the layers back-to-front: ascending `z`, with equal-`z` layers in
    /// their insertion order (a stable sort).
    pub fn draw_order(&self) -> impl Iterator<Item = &OverlayLayer> {
        let mut ordered: Vec<&OverlayLayer> = self.layers.iter().collect();
        ordered.sort_by_key(|layer| layer.z);
        ordered.into_iter()
    }
}
