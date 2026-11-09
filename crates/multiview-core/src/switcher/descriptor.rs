//! Switcher descriptor types: transitions, keyers and bus taps.
//!
//! The shapes pinned by ADR-0055 §3 (transition taxonomy and exact parameter
//! sets) and ADR-0056 §1/§3/§6 (canvas order, keyer model, DSK behaviour).
//!
//! Discipline, repo-wide: every union is serde **internally tagged** and the
//! enums are `#[non_exhaustive]`, so later waves can add kinds without breaking
//! callers. `untagged` is never used (fragile, poor errors, no forward
//! compatibility — ADR-0010).
//!
//! Timing lives in [`super::timing`]: durations here are
//! [`FrameCount`]s at the program's exact rational cadence, and the pixel-space
//! parameters (`clip`, `gain`, `softness`, `border_width`, `position_xy`) are
//! floats only because pixels are approximate — they are never time.
use crate::switcher::timing::FrameCount;
use crate::time::Rational;
use serde::{Deserialize, Serialize};

/// A reference to a bus-selectable source (or bound media player) by its stable
/// id (ADR-0055 §3).
///
/// The config layer owns the registry; the switcher descriptors carry only the
/// id, as [`crate::layout::Cell::source`] does.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceRef(String);

impl SourceRef {
    /// Wrap a source id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An integer-pixel rectangle in the key's destination space (ADR-0056 §3).
///
/// Used for the rectangular garbage matte and for the MVP `dve_pip` box key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PixelRect {
    /// Left edge in pixels (may be negative, i.e. off-canvas).
    pub x: i32,
    /// Top edge in pixels (may be negative, i.e. off-canvas).
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl PixelRect {
    /// Build a rectangle from its pixel geometry.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// A closed-form SDF wipe mask family (ADR-0055 §3, wave 2).
///
/// The variants mirror the house per-pixel primitives (linear edge, box,
/// circle); later patterns are additive because the enum is
/// `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WipePattern {
    /// A straight edge sweeping across the canvas.
    LinearEdge,
    /// A rectangle growing from the pattern origin.
    Box,
    /// A circle/ring growing from the pattern origin.
    Circle,
}

/// The travel direction of a DVE push or squeeze (ADR-0055 §3, wave 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DveDirection {
    /// Travel toward the left edge.
    Left,
    /// Travel toward the right edge.
    Right,
    /// Travel toward the top edge.
    Up,
    /// Travel toward the bottom edge.
    Down,
}

/// An armed M/E transition with its exact parameters (ADR-0055 §3).
///
/// Serde is internally tagged on `kind` and the enum is `#[non_exhaustive]`.
/// Every duration is a [`FrameCount`]; an API `rate_ms` converts exactly once at
/// the boundary and is never stored (ADR-T015 §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TransitionDescriptor {
    /// Atomic frame-boundary swap — the degenerate one-frame case of every timed
    /// transition below.
    Cut,
    /// Dissolve weighted by exact rational progress.
    Mix {
        /// Transition duration in output frames.
        rate: FrameCount,
    },
    /// Two chained mixes through a third source.
    Dip {
        /// Transition duration in output frames (at least 2; validated at arm).
        rate: FrameCount,
        /// The bus-selectable source dipped through.
        dip_source: SourceRef,
        /// Where the outgoing/incoming swap happens under the dip: an exact
        /// rational in `(0, 1)`, defaulting to `1/2`.
        #[serde(default = "default_switch_point")]
        switch_point: Rational,
    },
    /// Per-tile SDF mask transition (wave 2).
    Wipe {
        /// The closed-form mask family.
        pattern: WipePattern,
        /// Transition duration in output frames.
        rate: FrameCount,
        /// Edge softness (canvas-relative; `0` is a hard edge).
        #[serde(default)]
        softness: f32,
        /// Width of the optional border band (canvas-relative; `0` is none).
        #[serde(default)]
        border_width: f32,
        /// Source filling the border band; absent means no band.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        border_fill_source: Option<SourceRef>,
        /// Origin of origin-positionable patterns, normalized to the canvas.
        #[serde(default = "default_position_xy")]
        position_xy: [f32; 2],
        /// Mirror the travel direction.
        #[serde(default)]
        reverse: bool,
        /// Alternate the travel direction on successive takes.
        #[serde(default)]
        flip_flop: bool,
    },
    /// Animated translate (wave 2).
    DvePush {
        /// Travel direction.
        direction: DveDirection,
        /// Transition duration in output frames.
        rate: FrameCount,
    },
    /// Animated scale (wave 2).
    DveSqueeze {
        /// Travel direction.
        direction: DveDirection,
        /// Transition duration in output frames.
        rate: FrameCount,
    },
    /// A media transition rolling an alpha clip over the M/E (wave 3).
    Stinger {
        /// The bound media player's id.
        player: SourceRef,
        /// Frames the clip is pre-rolled before the take.
        pre_roll: FrameCount,
        /// The clip's length in frames.
        clip_duration: FrameCount,
        /// The frame the underlying mix starts on.
        trigger_point: FrameCount,
        /// The mix window in frames. `trigger_point + mix_rate <= clip_duration`
        /// is validated at arm time; `mix_rate = 1` is a hard cut point.
        mix_rate: FrameCount,
    },
}

/// The default dip split point: half the window (ADR-0055 §3).
fn default_switch_point() -> Rational {
    Rational::new(1, 2)
}

/// The default pattern origin: canvas centre.
fn default_position_xy() -> [f32; 2] {
    [0.5, 0.5]
}

/// The element set the next transition moves (ADR-0055 §7):
/// `{background} ∪ {key[1..n]}`.
///
/// At least one element must be armed when a transition is armed, and the
/// switcher value machine validates that. An empty set is representable here so
/// that "nothing armed" can be reported honestly instead of mis-typed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ArmedSet {
    /// Whether the background scene change is armed.
    #[serde(default)]
    pub background: bool,
    /// The 1-based USK indices armed to toggle through the transition.
    #[serde(default)]
    pub keys: Vec<u32>,
}

impl ArmedSet {
    /// Whether nothing is armed (valid to report, invalid to take).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.background && self.keys.is_empty()
    }

    /// Arm the background scene change.
    #[must_use]
    pub fn with_background(mut self) -> Self {
        self.background = true;
        self
    }

    /// Arm the USK at `index` (1-based).
    #[must_use]
    pub fn with_key(mut self, index: u32) -> Self {
        self.keys.push(index);
        self
    }
}

/// A keyer's fill and optional key source (ADR-0056 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KeySource {
    /// The fill — the picture the keyer shows.
    pub fill: SourceRef,
    /// The key/matte source; `None` means the keyer self-keys the fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<SourceRef>,
    /// Whether the fill is already premultiplied (shaped).
    ///
    /// Defaults to `false`, for raw fill+key pairs and for library media
    /// carrying **straight** alpha alike: the kernel premultiplies exactly once,
    /// in linear light. `true` is reserved for externally delivered shaped fills
    /// the source explicitly declares as such.
    #[serde(default)]
    pub premultiplied: bool,
}

impl KeySource {
    /// A self-keying fill with straight alpha.
    #[must_use]
    pub fn new(fill: SourceRef) -> Self {
        Self {
            fill,
            key: None,
            premultiplied: false,
        }
    }

    /// Pair the fill with an explicit key/matte source.
    #[must_use]
    pub fn with_key(mut self, key: SourceRef) -> Self {
        self.key = Some(key);
        self
    }
}

/// How a keyer derives its alpha (ADR-0056 §3).
///
/// Internally tagged on `kind`, never untagged, and `#[non_exhaustive]`: the
/// post-MVP chroma and pattern keys join once their parameter sets are pinned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum KeyType {
    /// Threshold on post-range-expand, normalized code-value luma.
    Luma {
        /// Clip point in `[0, 1]` on normalized code-value Y.
        #[serde(default)]
        clip: f32,
        /// Ramp gain (`>= 0`).
        #[serde(default = "default_key_gain")]
        gain: f32,
        /// Invert the resulting alpha.
        #[serde(default)]
        invert: bool,
    },
    /// A linear/alpha key read from the fill's or the key source's alpha plane.
    Linear,
    /// A scaled-box key: an integer-pixel destination rectangle (MVP).
    DvePip {
        /// The destination rectangle.
        rect: PixelRect,
    },
}

/// The default luma-key ramp gain: a straight clip with no expansion.
fn default_key_gain() -> f32 {
    1.0
}

/// The frame-boundary behaviour of a keyer (ADR-0056 §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum KeyerMode {
    /// Flip the on-air state at the next frame boundary.
    #[default]
    Cut,
    /// Ramp alpha over the keyer's own rate.
    Auto,
    /// Join the next M/E background transition instead of using its own rate.
    Tie,
}

/// A keyer's CUT/AUTO/TIE behaviour and, for AUTO, its rate (ADR-0056 §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KeyerTransition {
    /// The behaviour.
    #[serde(default)]
    pub mode: KeyerMode,
    /// The AUTO ramp rate in output frames; required when `mode` is
    /// [`KeyerMode::Auto`], validated at arm time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<FrameCount>,
}

impl KeyerTransition {
    /// A CUT: one frame, no ramp.
    #[must_use]
    pub const fn cut() -> Self {
        Self {
            mode: KeyerMode::Cut,
            rate: None,
        }
    }

    /// An AUTO ramp of `rate` frames.
    #[must_use]
    pub const fn auto(rate: FrameCount) -> Self {
        Self {
            mode: KeyerMode::Auto,
            rate: Some(rate),
        }
    }

    /// A TIE, riding the next background transition.
    #[must_use]
    pub const fn tie() -> Self {
        Self {
            mode: KeyerMode::Tie,
            rate: None,
        }
    }
}

/// One keyer descriptor: sources, key type, priority and behaviour
/// (ADR-0056 §3/§6).
///
/// The key type is flattened alongside the keyer's own fields, the
/// `multiview_config::Source` / `SourceKind` precedent — a luma keyer reads
/// `{ "z": 1, "source": { … }, "kind": "luma", "clip": 0.0, … }`. Key priority
/// is the explicit `z`, never an implicit "keyer 2 above keyer 1" rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KeyerDescriptor {
    /// Explicit z-order (higher draws on top).
    pub z: i32,
    /// The keyer's fill and optional key source.
    pub source: KeySource,
    /// The alpha derivation, flattened into this struct.
    #[serde(flatten)]
    pub key_type: KeyType,
    /// Rectangular garbage matte; alpha is forced to zero outside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub garbage_matte: Option<PixelRect>,
    /// Whether the keyer is currently contributing pixels.
    #[serde(default)]
    pub on_air: bool,
    /// The keyer's CUT/AUTO/TIE behaviour.
    #[serde(default)]
    pub transition: KeyerTransition,
}

impl KeyerDescriptor {
    /// A keyer at `z` with `source` and `key_type`, off air, CUT behaviour.
    #[must_use]
    pub fn new(z: i32, source: KeySource, key_type: KeyType) -> Self {
        Self {
            z,
            source,
            key_type,
            garbage_matte: None,
            on_air: false,
            transition: KeyerTransition::cut(),
        }
    }
}

/// A named tap point along one program's render plan (ADR-0056 §1).
///
/// Internally tagged on `tap`, never untagged, and `#[non_exhaustive]`: the
/// post-MVP key-fill / key-alpha aux taps extend the same set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "tap", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TapPoint {
    /// The post-FTB program output (dirty).
    Program,
    /// Post-DSK, pre-FTB (dirty).
    Dirty,
    /// The pre-DSK clean feed.
    Clean,
    /// The PVW composite.
    Preview,
    /// A specific M/E stage output.
    Me {
        /// The 0-based M/E index (the MVP ships one).
        index: u32,
    },
    /// An aux bus.
    Aux {
        /// The 0-based aux bus index.
        index: u32,
    },
}
