//! Per-stream decoupled routing — the crosspoint table (ADR-0034 / RT-4).
//!
//! The broadcast **router + multiviewer crosspoint** model: inputs, layouts and
//! outputs are independent resources wired by **per-stream** crosspoints. An
//! input is a bundle of elementary streams (1+ video, multiple audio tracks,
//! subtitle/caption tracks, SCTE-35/KLV data, timecode), so a crosspoint
//! addresses **one elementary stream of one input** ([`StreamRef`]) and binds it
//! to a destination (a video cell, an audio bus channel / discrete track, a
//! subtitle layer, or an output ← program).
//!
//! The three TIER-1 → TIER-2 maps ([`RoutingTable::video`],
//! [`RoutingTable::audio`], [`RoutingTable::subtitle`]) are **independently
//! keyed**, so *breakaway* (video from input A, audio track 2 from input B,
//! subtitles from input C) falls out structurally — nothing forces the audio or
//! subtitle key to equal the video cell's key.
//!
//! ## Backward compatibility — the desugar
//!
//! The routing table is a **sugar-superset** of today's single-program config.
//! When a document carries no explicit `[routing]` block, the equivalent
//! crosspoints are **derived** from the existing fields
//! ([`crate::MultiviewConfig::routing_table`]):
//!
//! * each `[[cells]]` with a `source.input_id` →
//!   [`VideoCrosspoint`]`{ cell, StreamRef{ input_id, Video, Best } }`;
//! * each `[audio].routes` entry →
//!   [`AudioCrosspoint`]`{ target, StreamRef{ input_id, Audio, Best } }`
//!   (composing with — never contradicting — the existing [`crate::AudioRouting`]
//!   block, ADR-R005);
//! * each `[[outputs]]` → [`OutputCrosspoint`]`{ output, program: "main" }`
//!   (the single-program default until ADR-0030's `ProgramSet` lands).
//!
//! A v1/v2 document and its desugared v3 form therefore solve to **identical**
//! routing.
//!
//! ## Serde discipline (ADR-0010)
//!
//! [`StreamSelector`] is **internally tagged** by `by` (`#[serde(tag = "by")]`),
//! **never `untagged`** — the only encoding robust across the self-describing
//! JSON wire form and non-self-describing TOML. [`StreamRef`] composes
//! [`multiview_core::stream::StreamKind`] (adjacently tagged by `kind` /
//! `payload`, from RT-0).

use std::collections::HashSet;

use multiview_core::stream::StreamKind;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// How a [`StreamRef`] picks **which** elementary stream of the requested
/// [`StreamKind`] to use, internally tagged by `by` (never `untagged`).
///
/// Resolution against the input's probed stream inventory happens at
/// **admission / runtime**, not config-time: a [`StreamSelector::Language`] or
/// [`StreamSelector::Index`] that does not (yet) resolve is **not** a config
/// error — the input may not have been probed, or the stream may appear after a
/// reconnect. Config-time validation only checks structural references
/// (the `input_id` and the destination exist).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamSelector {
    /// Select the stream at this container index **within its kind** (0-based).
    /// Fragile across re-probe / PMT-version bump / rendition reorder — the
    /// operator-visible reorder risk a stable id avoids.
    Index {
        /// The 0-based index among same-kind streams.
        index: usize,
    },
    /// Select the first stream whose BCP-47 / ISO-639 language tag matches
    /// (e.g. `"eng"`, `"fr-CA"`). The string is carried verbatim and validated
    /// against the input's inventory at admission, not here.
    Language {
        /// The requested BCP-47 / ISO-639 language tag.
        language: String,
    },
    /// Let the input pick its best stream of the requested kind (the default,
    /// and what the legacy desugar uses). The most permissive selector.
    Best,
    /// Select by a stable, kind-scoped id (`StableStreamId`, RT-0) — the
    /// reorder-proof key (TS PID / HLS group+name / general hash).
    StreamId {
        /// The stable stream id string (e.g. `"v/pid:256"`).
        id: String,
    },
}

impl Default for StreamSelector {
    /// The default selector is [`StreamSelector::Best`] — the most permissive,
    /// and the one the legacy desugar uses.
    fn default() -> Self {
        Self::Best
    }
}

impl StreamSelector {
    /// A by-index selector (mirrors the brief's `Index(usize)` shorthand).
    #[must_use]
    pub const fn index(index: usize) -> Self {
        Self::Index { index }
    }

    /// A by-language selector (mirrors the brief's `Language(String)`).
    #[must_use]
    pub const fn language(language: String) -> Self {
        Self::Language { language }
    }

    /// A by-stable-id selector (mirrors the brief's `StreamId(String)`).
    #[must_use]
    pub const fn stream_id(id: String) -> Self {
        Self::StreamId { id }
    }
}

/// The default selector for serde (`#[serde(default)]` needs a free function).
fn default_selector() -> StreamSelector {
    StreamSelector::Best
}

/// A reference to **one elementary stream of one input** — the
/// `StreamEndpoint = (input_id, StreamKind, selector)` of the brief.
///
/// This is the per-stream key a crosspoint binds its source to. The `kind`
/// composes [`multiview_core::stream::StreamKind`] (RT-0), so a crosspoint can
/// address a video, audio, subtitle, data (SCTE-35 / KLV) or timecode stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct StreamRef {
    /// The managed input id (`sources[].id`) this stream comes from.
    pub input_id: String,
    /// The canonical kind of the elementary stream (RT-0 `StreamKind`).
    pub kind: StreamKind,
    /// Which stream of that kind to use. Absent ⇒ [`StreamSelector::Best`].
    #[serde(default = "default_selector")]
    pub selector: StreamSelector,
}

impl StreamRef {
    /// Construct a `StreamRef` addressing the input's **best** stream of `kind`
    /// — the selector the legacy desugar uses.
    #[must_use]
    pub fn best(input_id: impl Into<String>, kind: StreamKind) -> Self {
        Self {
            input_id: input_id.into(),
            kind,
            selector: StreamSelector::Best,
        }
    }
}

/// One **video** crosspoint: a layout `cell` ← a video [`StreamRef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VideoCrosspoint {
    /// The destination layout cell id (`cells[].id`).
    pub cell: String,
    /// The source elementary stream feeding the cell.
    pub source: StreamRef,
}

/// One **audio** crosspoint: a program-bus channel or named discrete track
/// ← an audio [`StreamRef`].
///
/// The [`target`](AudioCrosspoint::target) names either the mixed program bus
/// ([`crate::PROGRAM_TRACK`], `"prog"`) or a named discrete output track — the
/// **same** track universe the [`crate::AudioRouting`] block (ADR-R005) declares.
/// This crosspoint composes with that block (it is the per-stream view of the
/// same routing), it does not introduce a second, conflicting audio model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AudioCrosspoint {
    /// The destination: a program-bus channel name or a named discrete track.
    pub target: String,
    /// The source elementary stream feeding the target.
    pub source: StreamRef,
    /// Program-bus contribution gain in dB (a level, not a rate; `0.0` ⇒ unity).
    /// Mirrors [`crate::AudioRoute::gain_db`]; defaults to unity.
    #[serde(default)]
    pub gain_db: f32,
    /// Whether the source contributes silence (still routed). Mirrors
    /// [`crate::AudioRoute::mute`].
    #[serde(default)]
    pub mute: bool,
}

/// One **subtitle** crosspoint: an overlay/caption `layer` ← a subtitle
/// [`StreamRef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SubtitleCrosspoint {
    /// The destination subtitle layer id (an overlay id, today).
    pub layer: String,
    /// The source elementary stream feeding the layer.
    pub source: StreamRef,
}

/// One **output** crosspoint: an `output` ← a `program`.
///
/// The `program` defaults to [`MAIN_PROGRAM`] (`"main"`) — the single-program
/// world today; ADR-0030's `ProgramSet` will make multiple programs addressable,
/// at which point a non-`"main"` program becomes meaningful.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OutputCrosspoint {
    /// The destination output, addressed by its stable [`crate::Output::id`]
    /// (the explicit operator id, or the derived label for a v1/v2 output that
    /// declares none — ADR-0034 / RT-12).
    pub output: String,
    /// The program this output carries. Absent ⇒ [`MAIN_PROGRAM`].
    #[serde(default = "main_program")]
    pub program: String,
}

impl OutputCrosspoint {
    /// View this crosspoint as a standalone [`OutputRef`] handle (the
    /// addressable `(output, program)` pair a runtime `RouteOutput` carries).
    #[must_use]
    pub fn as_ref(&self) -> OutputRef {
        OutputRef::new(self.output.clone(), self.program.clone())
    }
}

/// The single-program default program name until ADR-0030's `ProgramSet` lands.
pub const MAIN_PROGRAM: &str = "main";

/// The default program name for serde (`#[serde(default)]` needs a free fn).
fn main_program() -> String {
    MAIN_PROGRAM.to_owned()
}

/// A reference to **one output's program** — `(output, program)` (ADR-0034 /
/// RT-12).
///
/// This is the TIER-2 → TIER-3 join the brief calls an *`OutputRef`*: it names
/// a configured output (by its stable [`crate::Output::id`]) and the program it
/// carries. The `program` defaults to [`MAIN_PROGRAM`] (`"main"`) — the
/// single-program world today; ADR-0030's `ProgramSet` will make multiple
/// programs addressable, at which point a non-`"main"` program becomes
/// meaningful.
///
/// It is the standalone, addressable sibling of [`OutputCrosspoint`] (which is a
/// row in the [`RoutingTable`]): an `OutputRef` is the handle a runtime
/// `RouteOutput` command (RT-11) carries. Like every routing union it is
/// **never untagged** (here, a plain two-field struct with `deny_unknown_fields`),
/// robust across the self-describing JSON wire form and non-self-describing TOML
/// (ADR-0010).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OutputRef {
    /// The destination output's stable id ([`crate::Output::id`]).
    pub output: String,
    /// The program this output carries. Absent ⇒ [`MAIN_PROGRAM`].
    #[serde(default = "main_program")]
    pub program: String,
}

impl OutputRef {
    /// Construct an `OutputRef` for `output` carrying `program`.
    #[must_use]
    pub fn new(output: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            program: program.into(),
        }
    }

    /// Construct an `OutputRef` for `output` carrying the default
    /// [`MAIN_PROGRAM`] (`"main"`) — the single-program world until ADR-0030's
    /// `ProgramSet` lands.
    #[must_use]
    pub fn to_main(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            program: MAIN_PROGRAM.to_owned(),
        }
    }
}

/// The per-program **crosspoint table** (the `RouteMatrix` of the brief).
///
/// Four independently-keyed maps of crosspoints. An absent top-level `[routing]`
/// block desugars to an equivalent table; an explicit one is validated against
/// the document's declared cells / tracks / layers / outputs.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RoutingTable {
    /// Video crosspoints: cell ← video stream.
    #[serde(default)]
    pub video: Vec<VideoCrosspoint>,
    /// Audio crosspoints: bus channel / discrete track ← audio stream.
    #[serde(default)]
    pub audio: Vec<AudioCrosspoint>,
    /// Subtitle crosspoints: layer ← subtitle stream.
    #[serde(default)]
    pub subtitle: Vec<SubtitleCrosspoint>,
    /// Output crosspoints: output ← program.
    #[serde(default)]
    pub output: Vec<OutputCrosspoint>,
}

impl RoutingTable {
    /// Validate this table against the document's declared destinations and
    /// inputs.
    ///
    /// Enforces (structural references only — selector resolution is deferred to
    /// admission, so a [`StreamSelector::Language`]/[`StreamSelector::Index`]
    /// that does not resolve is **not** an error here):
    ///
    /// * every crosspoint's `source.input_id` resolves to a declared source;
    /// * every video crosspoint's `cell` resolves to a declared cell, and the
    ///   `source.kind` is [`StreamKind::Video`];
    /// * every audio crosspoint's `target` is a selectable track (the program
    ///   bus or a declared discrete track), and the `source.kind` is
    ///   [`StreamKind::Audio`], with a finite `gain_db`;
    /// * every subtitle crosspoint's `layer` resolves to a declared layer, and
    ///   the `source.kind` is [`StreamKind::Subtitle`];
    /// * every output crosspoint's `output` resolves to a declared output;
    /// * no cell / track / layer / output is bound by two crosspoints.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the first violated invariant.
    pub fn validate(&self, refs: &RoutingRefs<'_>) -> Result<(), ConfigError> {
        self.validate_video(refs)?;
        self.validate_audio(refs)?;
        self.validate_subtitle(refs)?;
        self.validate_output(refs)?;
        Ok(())
    }

    fn validate_source(refs: &RoutingRefs<'_>, source: &StreamRef) -> Result<(), ConfigError> {
        if !refs.sources.contains(source.input_id.as_str()) {
            return Err(ConfigError::Validation(format!(
                "routing crosspoint binds unknown source input_id {:?}",
                source.input_id
            )));
        }
        Ok(())
    }

    fn validate_video(&self, refs: &RoutingRefs<'_>) -> Result<(), ConfigError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.video.len());
        for xp in &self.video {
            Self::validate_source(refs, &xp.source)?;
            if xp.source.kind != StreamKind::Video {
                return Err(ConfigError::Validation(format!(
                    "video crosspoint for cell {:?} sources a non-video stream ({:?})",
                    xp.cell, xp.source.kind
                )));
            }
            if !refs.cells.contains(xp.cell.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "video crosspoint binds unknown cell {:?}",
                    xp.cell
                )));
            }
            if !seen.insert(xp.cell.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "two video crosspoints bind the same cell {:?}",
                    xp.cell
                )));
            }
        }
        Ok(())
    }

    fn validate_audio(&self, refs: &RoutingRefs<'_>) -> Result<(), ConfigError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.audio.len());
        for xp in &self.audio {
            Self::validate_source(refs, &xp.source)?;
            if xp.source.kind != StreamKind::Audio {
                return Err(ConfigError::Validation(format!(
                    "audio crosspoint for target {:?} sources a non-audio stream ({:?})",
                    xp.target, xp.source.kind
                )));
            }
            if !xp.gain_db.is_finite() {
                return Err(ConfigError::Validation(format!(
                    "audio crosspoint for target {:?}: gain_db must be finite (got {})",
                    xp.target, xp.gain_db
                )));
            }
            if !refs.tracks.contains(xp.target.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "audio crosspoint binds unknown track/bus target {:?}",
                    xp.target
                )));
            }
            if !seen.insert(xp.target.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "two audio crosspoints bind the same target {:?}",
                    xp.target
                )));
            }
        }
        Ok(())
    }

    fn validate_subtitle(&self, refs: &RoutingRefs<'_>) -> Result<(), ConfigError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.subtitle.len());
        for xp in &self.subtitle {
            Self::validate_source(refs, &xp.source)?;
            if xp.source.kind != StreamKind::Subtitle {
                return Err(ConfigError::Validation(format!(
                    "subtitle crosspoint for layer {:?} sources a non-subtitle stream ({:?})",
                    xp.layer, xp.source.kind
                )));
            }
            if !refs.layers.contains(xp.layer.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "subtitle crosspoint binds unknown layer {:?}",
                    xp.layer
                )));
            }
            if !seen.insert(xp.layer.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "two subtitle crosspoints bind the same layer {:?}",
                    xp.layer
                )));
            }
        }
        Ok(())
    }

    fn validate_output(&self, refs: &RoutingRefs<'_>) -> Result<(), ConfigError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.output.len());
        for xp in &self.output {
            if xp.program.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "output crosspoint for {:?} has an empty program",
                    xp.output
                )));
            }
            if !refs.outputs.contains(xp.output.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "output crosspoint binds unknown output {:?}",
                    xp.output
                )));
            }
            if !seen.insert(xp.output.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "two output crosspoints bind the same output {:?}",
                    xp.output
                )));
            }
        }
        Ok(())
    }
}

/// The document-derived **reference universe** a [`RoutingTable`] validates
/// against: the declared source ids, cell ids, selectable audio tracks,
/// subtitle layer ids, and output labels. Borrowed so validation allocates no
/// owned copies of the id strings.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RoutingRefs<'a> {
    /// Declared source ids (`sources[].id`).
    pub sources: HashSet<&'a str>,
    /// Declared cell ids (`cells[].id`).
    pub cells: HashSet<&'a str>,
    /// Selectable audio tracks (program bus + named discrete tracks).
    pub tracks: HashSet<&'a str>,
    /// Declared subtitle layer ids (overlay ids today).
    pub layers: HashSet<&'a str>,
    /// Declared output ids ([`crate::Output::id`] — explicit operator id, or the
    /// derived label).
    pub outputs: HashSet<&'a str>,
}
