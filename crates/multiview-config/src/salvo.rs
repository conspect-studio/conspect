//! Salvo configuration (config-as-code): named atomic recalls.
//!
//! A [`Salvo`] is an operator-armed, atomically-taken bundle of state changes
//! (broadcast-multiviewer brief §8): recall a named **layout**, rebind a set of
//! **sources** into cells, force a set of **tally** states, and set **UMD**
//! label text — all applied as one make-before-break swap so the wall never
//! shows a half-applied state. This crate owns the declarative shape and its
//! reference validation; the arm/take execution lives in `multiview-engine`.
//!
//! All unions are **internally tagged** by `kind`, never `untagged` (ADR-0010).
//! The one exception in shape is a patch crosspoint's `source`
//! ([`PatchSourceSpec`]), which keeps the patch API's exact JSON
//! (`{"source": id}` | `{"layout": id}` | `{"bus": ..}` | `"follow"` | `"none"`,
//! output-routing §12.8): it is (de)serialized by hand, never `untagged`.

use std::collections::HashSet;

use multiview_core::tally::TallyColor;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::routing::OutputBus;

/// Most crosspoints one patch set (and so one patch salvo) carries
/// (output-routing §12.8).
pub const MAX_PATCH_CROSSPOINTS: usize = 64;

/// Rebind one cell to a managed source as part of a salvo recall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SourceRecall {
    /// The cell id whose source binding changes.
    pub cell: String,
    /// The managed source id to bind into the cell.
    pub input_id: String,
}

/// Force a cell's tally lamp to a fixed colour as part of a salvo recall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TallyRecall {
    /// The cell id whose tally is forced.
    pub cell: String,
    /// The lamp colour to assert.
    pub color: TallyColor,
}

/// Set a cell's Under-Monitor Display label text as part of a salvo recall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct UmdRecall {
    /// The cell id whose UMD label changes.
    pub cell: String,
    /// The label text to display.
    pub text: String,
}

/// Where a patch crosspoint lands (output-routing §12.8): a mixer input
/// (`mixer` + `input`) or an output slot (`output` + `slot`, default 0).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PatchDestinationSpec {
    /// M/E id (`me1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mixer: Option<String>,
    /// Mixer input number, 1..=64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u16>,
    /// Output API id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Output slot index (0 when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
}

impl PatchDestinationSpec {
    /// A mixer input destination.
    #[must_use]
    pub fn mixer(me: impl Into<String>, input: u16) -> Self {
        Self {
            mixer: Some(me.into()),
            input: Some(input),
            ..Self::default()
        }
    }

    /// An output slot destination.
    #[must_use]
    pub fn output(output: impl Into<String>, slot: u32) -> Self {
        Self {
            output: Some(output.into()),
            slot: Some(slot),
            ..Self::default()
        }
    }
}

/// What a patch crosspoint patches (output-routing §12.8). Its JSON is the
/// patch API's own: `{"source": id}` (optionally `"track": i`),
/// `{"layout": id}`, `{"bus": "program"|"preview"|"clean"}`, `"follow"` or
/// `"none"`. Hand-written serde (ADR-0010 forbids `untagged`); an unknown
/// shape is a deserialization error.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PatchSourceSpec {
    /// One source, optionally one of its tracks (audio/subtitle planes).
    Source {
        /// The source id.
        id: String,
        /// The source's track index, when the plane selects one.
        track: Option<u32>,
    },
    /// A stored layout.
    Layout(String),
    /// A switcher bus.
    Bus(OutputBus),
    /// The audio of the slot's video (audio slots).
    Follow,
    /// Nothing (an unpatched mixer input).
    None,
}

impl PatchSourceSpec {
    /// The patch API JSON of this source.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Self::Source { id, track: None } => json!({ "source": id }),
            Self::Source {
                id,
                track: Some(track),
            } => json!({ "source": id, "track": track }),
            Self::Layout(id) => json!({ "layout": id }),
            Self::Bus(bus) => json!({ "bus": bus }),
            Self::Follow => json!("follow"),
            Self::None => json!("none"),
        }
    }

    /// Parse the patch API JSON of a source.
    ///
    /// # Errors
    /// A description of the unknown shape.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        use serde_json::Value;
        const SHAPES: &str = "a patch source is {\"source\": id}, {\"source\": id, \"track\": i}, \
                              {\"layout\": id}, {\"bus\": \"program\"|\"preview\"|\"clean\"}, \
                              \"follow\" or \"none\"";
        match value {
            Value::String(s) if s == "follow" => Ok(Self::Follow),
            Value::String(s) if s == "none" => Ok(Self::None),
            Value::Object(map) => {
                let only = |key: &str, extra: Option<&str>| {
                    map.keys().all(|k| k == key || Some(k.as_str()) == extra)
                };
                if let Some(id) = map.get("source") {
                    let id = id.as_str().ok_or(SHAPES)?;
                    if !only("source", Some("track")) {
                        return Err(SHAPES.to_owned());
                    }
                    let track = match map.get("track") {
                        None => None,
                        Some(t) => Some(
                            t.as_u64()
                                .and_then(|t| u32::try_from(t).ok())
                                .ok_or(SHAPES)?,
                        ),
                    };
                    return Ok(Self::Source {
                        id: id.to_owned(),
                        track,
                    });
                }
                if map.len() != 1 {
                    return Err(SHAPES.to_owned());
                }
                if let Some(id) = map.get("layout").and_then(Value::as_str) {
                    return Ok(Self::Layout(id.to_owned()));
                }
                if let Some(bus) = map.get("bus") {
                    return serde_json::from_value(bus.clone())
                        .map(Self::Bus)
                        .map_err(|_| "unknown bus; use program, preview or clean".to_owned());
                }
                Err(SHAPES.to_owned())
            }
            _ => Err(SHAPES.to_owned()),
        }
    }
}

impl Serialize for PatchSourceSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PatchSourceSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::from_json(&value).map_err(serde::de::Error::custom)
    }
}

/// One stored patch crosspoint `{ plane, destination, source }`
/// (output-routing §12.8), the shape `POST /patch/take` accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PatchCrosspoint {
    /// `video` or `audio` (`subtitle` arrives with #345).
    pub plane: String,
    /// Where the source is patched.
    pub destination: PatchDestinationSpec,
    /// What is patched.
    pub source: PatchSourceSpec,
}

impl PatchCrosspoint {
    /// A crosspoint.
    #[must_use]
    pub fn new(
        plane: impl Into<String>,
        destination: PatchDestinationSpec,
        source: PatchSourceSpec,
    ) -> Self {
        Self {
            plane: plane.into(),
            destination,
            source,
        }
    }
}

/// A patch salvo's on-air guard (output-routing §12.8): whether a mixer
/// crosspoint may re-patch the input on Program.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchOnAir {
    /// Refuse (`on-air-protected`) — the default.
    #[default]
    Protect,
    /// Apply it as a cut on Program.
    Allow,
}

impl PatchOnAir {
    #[allow(clippy::trivially_copy_pass_by_ref)]
    // reason: serde's `skip_serializing_if` passes a reference.
    fn is_protect(&self) -> bool {
        *self == Self::Protect
    }
}

/// A named, atomically-applied recall.
///
/// All four sub-recalls are optional, but a salvo must change **something**
/// (validation rejects an empty salvo). `layout` names a [`crate::schema::Layout`]
/// preset or a head layout by name; the source/tally/umd recalls reference cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Salvo {
    /// Stable salvo id (unique within the document).
    pub id: String,
    /// Human-friendly display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Recall a named layout (preset or head layout name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Source rebindings.
    #[serde(default)]
    pub sources: Vec<SourceRecall>,
    /// Forced tally states.
    #[serde(default)]
    pub tally: Vec<TallyRecall>,
    /// UMD label changes.
    #[serde(default)]
    pub umd: Vec<UmdRecall>,
    /// Patch crosspoints taken atomically at one tick through the patch
    /// set path (ADR-0110 decision 8, output-routing §12.8, #308).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patch: Vec<PatchCrosspoint>,
    /// The on-air guard of the patch crosspoints (default `protect`).
    #[serde(default, skip_serializing_if = "PatchOnAir::is_protect")]
    pub on_air: PatchOnAir,
}

impl Salvo {
    /// Validate this salvo's internal consistency (non-empty id, does something,
    /// no cell rebound to two sources, non-empty references).
    ///
    /// Cell- and source-reference resolution against the document is enforced by
    /// [`crate::MultiviewConfig::validate`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for an empty id, a salvo that changes
    /// nothing, a cell rebound to more than one source, or an empty cell/source
    /// reference.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.is_empty() {
            return Err(ConfigError::Validation(
                "a salvo has an empty id".to_owned(),
            ));
        }
        if self.layout.is_none()
            && self.sources.is_empty()
            && self.tally.is_empty()
            && self.umd.is_empty()
            && self.patch.is_empty()
        {
            return Err(ConfigError::Validation(format!(
                "salvo {:?} changes nothing (no layout, sources, tally, umd, or patch)",
                self.id
            )));
        }
        if let Some(layout) = &self.layout {
            if layout.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} has an empty layout name",
                    self.id
                )));
            }
        }
        let mut seen_cells: HashSet<&str> = HashSet::with_capacity(self.sources.len());
        for recall in &self.sources {
            if recall.cell.is_empty() || recall.input_id.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} has a source recall with an empty cell or input_id",
                    self.id
                )));
            }
            if !seen_cells.insert(recall.cell.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} rebinds cell {:?} more than once",
                    self.id, recall.cell
                )));
            }
        }
        for recall in &self.tally {
            if recall.cell.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} has a tally recall with an empty cell",
                    self.id
                )));
            }
        }
        for recall in &self.umd {
            if recall.cell.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} has a umd recall with an empty cell",
                    self.id
                )));
            }
        }
        self.validate_patch()
    }

    /// The patch members' own consistency: at most
    /// [`MAX_PATCH_CROSSPOINTS`], each destination exactly a mixer input or
    /// an output slot, no destination twice on one plane, non-empty ids.
    fn validate_patch(&self) -> Result<(), ConfigError> {
        let refuse = |detail: String| {
            Err(ConfigError::Validation(format!(
                "salvo {:?} patch: {detail}",
                self.id
            )))
        };
        if self.patch.len() > MAX_PATCH_CROSSPOINTS {
            return refuse(format!(
                "at most {MAX_PATCH_CROSSPOINTS} crosspoints, got {}",
                self.patch.len()
            ));
        }
        let mut seen: HashSet<(&str, PatchDestinationSpec)> =
            HashSet::with_capacity(self.patch.len());
        for xp in &self.patch {
            let d = &xp.destination;
            let mixer = d.mixer.is_some() || d.input.is_some();
            let output = d.output.is_some() || d.slot.is_some();
            match (mixer, output) {
                (true, true) => {
                    return refuse(
                        "a destination is a mixer input or an output slot, not both".into(),
                    )
                }
                (false, false) => return refuse("a crosspoint has no destination".into()),
                (true, false)
                    if d.mixer.as_deref().is_none_or(str::is_empty) || d.input.is_none() =>
                {
                    return refuse("a mixer destination needs both `mixer` and `input`".into())
                }
                (false, true) if d.output.as_deref().is_none_or(str::is_empty) => {
                    return refuse("an output slot destination needs `output`".into())
                }
                _ => {}
            }
            if !matches!(xp.plane.as_str(), "video" | "audio" | "subtitle") {
                return refuse(format!("unknown plane {:?}", xp.plane));
            }
            let empty = match &xp.source {
                PatchSourceSpec::Source { id, .. } | PatchSourceSpec::Layout(id) => id.is_empty(),
                _ => false,
            };
            if empty {
                return refuse("a crosspoint source has an empty id".into());
            }
            // An output slot without `slot` is slot 0: normalize for the
            // duplicate check.
            let normalized = if output && d.slot.is_none() {
                PatchDestinationSpec::output(d.output.clone().unwrap_or_default(), 0)
            } else {
                d.clone()
            };
            let label = format!("{normalized:?}");
            if !seen.insert((xp.plane.as_str(), normalized)) {
                return refuse(format!(
                    "the {} destination {label} appears twice",
                    xp.plane
                ));
            }
        }
        Ok(())
    }
}
