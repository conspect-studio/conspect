//! # multiview-config
//!
//! Config & template schema (serde), validation, and config-as-code
//! import/export for the Multiview engine.
//!
//! This crate parses the authored TOML (and the canonical JSON wire form)
//! described in `docs/templates/layout-and-config.md`, solves CSS-grid layouts
//! into normalized rectangles, and validates the document's semantic
//! invariants before it ever reaches the engine. It is pure Rust with no native
//! dependencies — it builds in the GPU-free CI baseline.
//!
//! ## Pipeline
//!
//! ```text
//! TOML/JSON ──load──▶ MultiviewConfig ──validate──▶ solve_layout ──▶ multiview_core::layout::Layout
//! ```
//!
//! - [`MultiviewConfig::load_from_toml`] / [`MultiviewConfig::load_from_json`] parse a
//!   document (rejecting a float `fps`, malformed track, etc. at parse time).
//! - [`MultiviewConfig::validate`] enforces the semantic invariants: unique ids,
//!   every `cells.source.input_id` resolves to a declared source, every grid
//!   `area` exists, the cadence is usable, and the **solved** geometry passes
//!   [`multiview_core::layout::Layout::validate`].
//! - [`MultiviewConfig::solve_layout`] flattens the document into a validated
//!   [`multiview_core::layout::Layout`] (canvas + normalized cells) for the engine.
//!
//! All unions are internally tagged by `kind` (never `untagged`) per ADR-0010.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod audio;
pub mod device;
pub mod discovery;
pub mod error;
pub mod failover;
pub mod grid;
pub mod layout_doc;
pub mod placement;
pub mod probe;
pub mod program;
pub mod routing;
pub mod salvo;
pub mod schema;
pub mod sync_group;
pub mod tally;
pub mod timer;
pub mod wall;

use std::collections::{HashMap, HashSet};

use multiview_core::layout::{
    Canvas as CoreCanvas, Cell as CoreCell, FitMode, Layout as CoreLayout,
};
use multiview_core::stream::StreamKind as CoreStreamKind;

use audio::PROGRAM_TRACK as PROGRAM_TRACK_NAME;
pub use audio::{
    AudioChannels, AudioRoute, AudioRouting, OutputAudio, OutputAudioCapability, OutputAudioMode,
    TrackCapacity, TrackDelivery, PROGRAM_TRACK,
};
pub use device::{Device, DeviceAuth, DeviceDisplay, DeviceDriver, DisplayAssign, ReconnectPolicy};
pub use discovery::DiscoveryConfig;
pub use error::ConfigError;
pub use failover::{default_failover_slate, FailoverSlate};
pub use layout_doc::{LayoutCanvas, LayoutDocument};
pub use placement::{DevicePin, MigrationPolicy, PinVendor, PlacementConfig, PlacementWeights};
pub use probe::{DetectionZone, Dwell, LoudnessTarget, Probe, ProbeKind};
pub use program::{ProgramId, ProgramKind, ProgramSpec};
pub use routing::{
    AudioCrosspoint, OutputCrosspoint, OutputRef, RoutingRefs, RoutingTable, StreamRef,
    StreamSelector, SubtitleCrosspoint, VideoCrosspoint, MAIN_PROGRAM,
};
pub use salvo::{Salvo, SourceRecall, TallyRecall, UmdRecall};
pub use schema::{
    Border, Canvas, CanvasColor, Cell, CellQos, CellSource, ClockFaceConfig, ColorOverride,
    DisplayModeSpec, Fps, Layout, Output, Overlay, Rect, RtspOptions, Source, SourceAuth,
    SourceKind,
};
pub use sync_group::{SyncGroup, SyncGroupMode, SyncMember};
pub use tally::{BitColor, IndexCell, TallyProfile};
pub use timer::{
    compute as compute_timer, decompose as decompose_duration, frame_index, overrun_badge_word,
    TimerDirection, TimerError, TimerFormat, TimerOnTarget, TimerReadout, TimerState, TimerTarget,
};
pub use wall::{HeadConfig, WallBezel, WallConfig};

/// The management control-plane listener.
///
/// When present, `multiview run` serves the REST + WebSocket + SSE API, the
/// OpenAPI/Scalar docs (`/docs`), and — when the control plane is built with its
/// `embed-web` feature — the web UI, all on [`listen`](ControlConfig::listen)
/// alongside the engine. Absent ⇒ today's headless behaviour (the daemon binds
/// no listener). The control plane is isolation-safe (invariant #10): it reads
/// the engine's wait-free state slot and drop-oldest event broadcast and submits
/// to the non-blocking command bus, so it can never back-pressure the engine.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct ControlConfig {
    /// The socket address to bind, e.g. `"[::]:8080"` (all interfaces, IPv6
    /// dual-stack — accepts IPv4-mapped clients too) or `"[::1]:8080"`
    /// (loopback). IPv6-first: prefer the `[::]`/`[::1]` forms; a user-supplied
    /// IPv4 address (`"127.0.0.1:8080"`) still parses and binds, but is not the
    /// default. Validated as a parseable [`std::net::SocketAddr`] by
    /// [`MultiviewConfig::validate`].
    pub listen: String,
}

/// A complete Multiview configuration document (config-as-code).
///
/// This is the whole-engine declarative state: canvas, layout strategy,
/// managed sources, cells, overlays, and outputs, plus a `schema_version` that
/// drives migration. It deserializes from TOML (human authoring) and JSON (the
/// canonical wire form) and round-trips losslessly between them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct MultiviewConfig {
    /// Document schema version (drives migration).
    pub schema_version: u32,
    /// The output canvas.
    pub canvas: Canvas,
    /// The layout placement strategy.
    pub layout: Layout,
    /// Managed inputs (owners of ingest/decode/color/resilience).
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Cells (tiles) and their source bindings.
    #[serde(default)]
    pub cells: Vec<Cell>,
    /// Overlay layers.
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    /// Output sinks/servers.
    #[serde(default)]
    pub outputs: Vec<Output>,
    /// Content-aware fault probes (black/freeze/silence/loudness), per cell.
    #[serde(default)]
    pub probes: Vec<Probe>,
    /// Tally profiles (bit↔colour palette, index↔cell address map).
    #[serde(default)]
    pub tally_profiles: Vec<TallyProfile>,
    /// Named atomic recalls (layout + source + tally + UMD).
    #[serde(default)]
    pub salvos: Vec<Salvo>,
    /// Multi-head video walls.
    #[serde(default)]
    pub walls: Vec<WallConfig>,
    /// Managed devices (ADR-M008): operator-adopted hardware — encoder/decoder
    /// appliances, display nodes, cast targets — as declarative **desired
    /// state**. Runtime status (online state, firmware, temperature, achieved
    /// skew) has no representation here and is never exported.
    #[serde(default)]
    pub devices: Vec<Device>,
    /// Presentation-sync groups over managed devices (ADR-M008 / ADR-M010):
    /// achieved tier = weakest member, per-member `offset_ms` trim, drift
    /// alarm beyond `target_skew_ms`.
    #[serde(default)]
    pub sync_groups: Vec<SyncGroup>,
    /// mDNS/DNS-SD discovery browse configuration (ADR-M008 §6): the
    /// operator-configured zowietek-control service type (the vendor's type is
    /// unverified — never fabricated) and any extra DNS-SD types to browse.
    /// Absent ⇒ only the built-in Cast + NDI types are browsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<DiscoveryConfig>,
    /// The management control-plane listener. When present, `multiview run`
    /// serves the API + docs (+ web UI) alongside the engine; absent ⇒ headless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ControlConfig>,
    /// The GPU work-placement policy (ADR-0018). Absent ⇒ the engine uses its
    /// conservative built-in defaults (single-GPU hosts add zero behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementConfig>,
    /// The audio routing block (ADR-R005): program-bus mix membership/gains and
    /// discrete per-input track wiring. Absent ⇒ no managed audio routing (the
    /// engine carries no audio for this document). The mix/encode/mux runtime
    /// that consumes these routes is `multiview-audio` + the engine (AUD-3/AUD-4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioRouting>,
    /// The per-stream decoupled-routing crosspoint table (ADR-0034 / RT-4): the
    /// broadcast router/multiviewer model where inputs, layouts and outputs are
    /// independent resources wired by per-stream crosspoints. **Absent ⇒ the
    /// legacy single-program path** — the equivalent crosspoints are derived
    /// from the existing `cells`/`audio`/`outputs` fields by
    /// [`MultiviewConfig::routing_table`] (the desugar), so a v1/v2 document and
    /// its desugared v3 form route identically. Schema v3 introduces this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingTable>,
}

/// Parse a `#RGB` / `#RRGGBB` hex color into its `(r, g, b)` bytes.
///
/// Returns `None` for anything that is not a `#`-prefixed 3- or 6-digit ASCII
/// hex triplet. Shared by config validation and the synthetic `solid` source
/// generator so that a config which validates renders the exact colour asked
/// for. Allocation-free and slicing-free (guardrail-clean for the data plane).
#[must_use]
pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Decode two ASCII hex digits into one byte without slicing.
    let byte = |hi: u8, lo: u8| -> Option<u8> {
        let buf = [hi, lo];
        let text = core::str::from_utf8(&buf).ok()?;
        u8::from_str_radix(text, 16).ok()
    };
    let b = hex.as_bytes();
    match b.len() {
        3 => {
            let r = *b.first()?;
            let g = *b.get(1)?;
            let bl = *b.get(2)?;
            Some((byte(r, r)?, byte(g, g)?, byte(bl, bl)?))
        }
        6 => Some((
            byte(*b.first()?, *b.get(1)?)?,
            byte(*b.get(2)?, *b.get(3)?)?,
            byte(*b.get(4)?, *b.get(5)?)?,
        )),
        _ => None,
    }
}

impl MultiviewConfig {
    /// Parse a configuration document from TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::TomlParse`] if the text is not valid TOML matching
    /// the schema (including a float `fps`, an unknown enum `kind`, or a
    /// malformed `fps` string).
    pub fn load_from_toml(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::TomlParse(e.to_string()))
    }

    /// Parse a configuration document from JSON text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::JsonParse`] if the text is not valid JSON matching
    /// the schema.
    pub fn load_from_json(text: &str) -> Result<Self, ConfigError> {
        serde_json::from_str(text).map_err(|e| ConfigError::JsonParse(e.to_string()))
    }

    /// Serialize this document to TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::TomlSerialize`] if serialization fails.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string(self).map_err(|e| ConfigError::TomlSerialize(e.to_string()))
    }

    /// Serialize this document to JSON text (the canonical wire form).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::JsonSerialize`] if serialization fails.
    pub fn to_json(&self) -> Result<String, ConfigError> {
        serde_json::to_string_pretty(self).map_err(|e| ConfigError::JsonSerialize(e.to_string()))
    }

    /// Validate every semantic invariant this crate owns.
    ///
    /// Enforces, in addition to [`multiview_core::layout::Layout::validate`] on the
    /// **solved** geometry:
    /// - `schema_version` is supported;
    /// - source ids are unique and non-empty;
    /// - cell ids are unique and non-empty;
    /// - every grid cell's `area` exists in the grid's area map, and every cell
    ///   is placed by exactly one of `area`/`rect`;
    /// - every `cells.source.input_id` resolves to a declared source id;
    /// - at least one output is declared, each with a non-empty codec;
    /// - the canvas cadence is a usable rational (invariant #3);
    /// - every probe/tally-profile/salvo references a declared cell (and every
    ///   salvo source-recall references a declared source), and each is
    ///   internally consistent;
    /// - every declared video wall passes the core wall invariants;
    /// - device ids are unique, each device satisfies its driver's
    ///   requirements (ADR-M008), and every display assignment resolves to a
    ///   declared output / wall head;
    /// - sync-group ids are unique, every member references a declared
    ///   device, no device belongs to two groups, and skew/offset bounds are
    ///   sane;
    /// - every `[discovery]` service type is a well-formed DNS-SD type
    ///   (`_name._tcp` / `_name._udp`, optionally `.local.`-suffixed).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] (or a more specific variant from the
    /// grid solver / cadence parse) naming the first violated invariant.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version == 0 {
            return Err(ConfigError::Validation(
                "schema_version must be >= 1".to_owned(),
            ));
        }

        self.validate_unique_ids()?;
        self.validate_sources()?;
        self.validate_cell_bindings()?;
        self.validate_outputs()?;
        self.validate_audio()?;
        self.validate_probes()?;
        self.validate_tally_profiles()?;
        self.validate_salvos()?;
        self.validate_walls()?;
        self.validate_devices()?;
        self.validate_sync_groups()?;
        self.validate_discovery()?;
        self.validate_control()?;
        self.validate_placement()?;
        self.validate_routing()?;

        // Solving + the core structural check covers geometry (rects in 0..1,
        // positive extent, valid cadence) and grid wiring (areas resolve).
        let layout = self.solve_layout()?;
        layout.validate().map_err(|e| match e {
            multiview_core::Error::Config(msg) => ConfigError::Validation(msg),
            other => ConfigError::Validation(other.to_string()),
        })?;

        Ok(())
    }

    /// Validate per-source kind-specific fields so a config that validates cannot
    /// fail at render time: a `solid` colour must be a parseable hex string, and a
    /// `clock` timezone offset must be a real UTC offset (`-720..=840` minutes).
    fn validate_sources(&self) -> Result<(), ConfigError> {
        for source in &self.sources {
            source.validate()?;
        }
        Ok(())
    }

    /// Validate the `[discovery]` browse configuration (ADR-M008 §6): every
    /// configured DNS-SD service type is well-formed
    /// ([`DiscoveryConfig::validate`]).
    fn validate_discovery(&self) -> Result<(), ConfigError> {
        match &self.discovery {
            Some(discovery) => discovery.validate(),
            None => Ok(()),
        }
    }

    /// Validate managed devices (ADR-M008): each is internally consistent
    /// ([`Device::validate`]), ids are unique, and every display assignment
    /// resolves — an `{ output = … }` ref to a declared output's stable id,
    /// a `{ wall_head = … }` ref to a head of a declared video wall.
    fn validate_devices(&self) -> Result<(), ConfigError> {
        let output_ids = self.output_ids();
        let outputs: HashSet<&str> = output_ids.iter().map(String::as_str).collect();
        let wall_heads: HashSet<&str> = self
            .walls
            .iter()
            .flat_map(|wall| wall.heads.iter().map(|head| head.id.as_str()))
            .collect();

        let mut seen: HashSet<&str> = HashSet::with_capacity(self.devices.len());
        for device in &self.devices {
            device.validate()?;
            if !seen.insert(device.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate device id {:?}",
                    device.id
                )));
            }
            if let Some(display) = &device.display {
                match &display.assign {
                    // The payload (must be `true`) is checked by `Device::validate`.
                    DisplayAssign::Program(_) => {}
                    DisplayAssign::Output(output) => {
                        if !outputs.contains(output.as_str()) {
                            return Err(ConfigError::Validation(format!(
                                "device {:?} display assignment references unknown output \
                                 {output:?}",
                                device.id
                            )));
                        }
                    }
                    DisplayAssign::WallHead(head) => {
                        if !wall_heads.contains(head.as_str()) {
                            return Err(ConfigError::Validation(format!(
                                "device {:?} display assignment references unknown wall head \
                                 {head:?}",
                                device.id
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Validate sync groups (ADR-M008 / ADR-M010): each is internally
    /// consistent ([`SyncGroup::validate`]), ids are unique, every member
    /// references a declared device, and a device belongs to **at most one**
    /// group (two groups disagreeing about one device's presentation trim is
    /// unsatisfiable).
    fn validate_sync_groups(&self) -> Result<(), ConfigError> {
        let devices: HashMap<&str, &DeviceDriver> = self
            .devices
            .iter()
            .map(|d| (d.id.as_str(), &d.driver))
            .collect();
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.sync_groups.len());
        // Which group already claimed each member device.
        let mut membership: HashMap<&str, &str> = HashMap::new();
        for group in &self.sync_groups {
            group.validate()?;
            if !seen.insert(group.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate sync group id {:?}",
                    group.id
                )));
            }
            for member in &group.members {
                match devices.get(member.device.as_str()) {
                    None => {
                        return Err(ConfigError::Validation(format!(
                            "sync group {:?} references unknown device {:?}",
                            group.id, member.device
                        )));
                    }
                    Some(DeviceDriver::Cast) => {
                        return Err(ConfigError::Validation(format!(
                            "sync group {:?} includes cast device {:?}: cast outputs are Tier D \
                             (seconds of receiver buffering, no sync surface) and are never sync \
                             participants (ADR-M011)",
                            group.id, member.device
                        )));
                    }
                    Some(_) => {}
                }
                if let Some(previous) = membership.insert(member.device.as_str(), &group.id) {
                    return Err(ConfigError::Validation(format!(
                        "device {:?} is a member of sync groups {previous:?} and {:?} (a \
                         device may belong to at most one sync group)",
                        member.device, group.id
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validate the optional control-plane listener: its `listen` must parse as a
    /// [`std::net::SocketAddr`], so a typo fails at config-validation time rather
    /// than when `multiview run` tries to bind the socket.
    fn validate_control(&self) -> Result<(), ConfigError> {
        if let Some(control) = &self.control {
            control
                .listen
                .parse::<std::net::SocketAddr>()
                .map_err(|e| {
                    ConfigError::Validation(format!(
                        "control.listen {:?} is not a valid socket address: {e}",
                        control.listen
                    ))
                })?;
        }
        Ok(())
    }

    /// Flatten this document into a validated-shape [`multiview_core::layout::Layout`].
    ///
    /// Grid cells are placed by solving the CSS grid; absolute cells use their
    /// declared `rect`. The returned layout is *structurally* assembled — call
    /// [`multiview_core::layout::Layout::validate`] (or [`MultiviewConfig::validate`])
    /// to enforce its invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Grid`] for an unsolvable grid, or
    /// [`ConfigError::Validation`] when a cell lacks a placement, references an
    /// unknown grid area, or a grid cell appears under a non-grid layout.
    pub fn solve_layout(&self) -> Result<CoreLayout, ConfigError> {
        let canvas = self.core_canvas();

        let grid_rects = match self.layout.as_grid_layout()? {
            Some(grid) => Some(grid::solve(&grid, self.canvas.width, self.canvas.height)?),
            None => None,
        };

        let mut cells = Vec::with_capacity(self.cells.len());
        for cell in &self.cells {
            let core_cell = Self::solve_cell(cell, grid_rects.as_deref())?;
            cells.push(core_cell);
        }

        Ok(CoreLayout {
            name: format!("schema_v{}", self.schema_version),
            canvas,
            cells,
        })
    }

    /// Build the core canvas (geometry + exact cadence) from this document.
    fn core_canvas(&self) -> CoreCanvas {
        let cadence = self.canvas.fps.rational();
        CoreCanvas {
            width: self.canvas.width,
            height: self.canvas.height,
            fps_num: cadence.num,
            fps_den: cadence.den,
        }
    }

    /// Resolve one schema cell into a core cell, placing it by grid area or
    /// absolute rect. Shared with [`layout_doc::LayoutDocument::solve_named`]
    /// (the stored named-layout solve, ADR-W019) so a stored body and the
    /// working config solve cells identically.
    pub(crate) fn solve_cell(
        cell: &Cell,
        grid_rects: Option<&[grid::AreaRect]>,
    ) -> Result<CoreCell, ConfigError> {
        let (x, y, w, h) = match (&cell.area, &cell.rect) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::Validation(format!(
                    "cell {:?} declares both `area` and `rect`; choose exactly one",
                    cell.id
                )));
            }
            (Some(area), None) => {
                let rects = grid_rects.ok_or_else(|| {
                    ConfigError::Validation(format!(
                        "cell {:?} uses grid `area` {area:?} but the layout is not a grid",
                        cell.id
                    ))
                })?;
                let found = rects.iter().find(|r| &r.name == area).ok_or_else(|| {
                    ConfigError::Validation(format!(
                        "cell {:?} references unknown grid area {area:?}",
                        cell.id
                    ))
                })?;
                (found.x, found.y, found.w, found.h)
            }
            (None, Some(rect)) => (rect.x, rect.y, rect.w, rect.h),
            (None, None) => {
                return Err(ConfigError::Validation(format!(
                    "cell {:?} has neither `area` nor `rect`",
                    cell.id
                )));
            }
        };

        Ok(CoreCell {
            x,
            y,
            w,
            h,
            z: cell.z,
            fit: parse_fit(cell.fit.as_deref()),
            source: cell.source.input_id.clone(),
            // Per-tile opacity: a fully-opaque hard-cover (1.0) when the document
            // omits it, matching the core `Cell` default. The compositor honours
            // it in the premultiplied linear-light `over` blend; out-of-range
            // values are rejected by `Layout::validate` downstream.
            opacity: cell.opacity.unwrap_or(1.0),
            // Broadcast per-tile crop/rotation are additive, defaulted fields on
            // the shared `Cell`; this mapper does not yet surface them from the
            // config schema, so spread their defaults.
            ..CoreCell::default()
        })
    }

    /// Reject empty/duplicate source and cell ids.
    fn validate_unique_ids(&self) -> Result<(), ConfigError> {
        let mut seen_sources: HashSet<&str> = HashSet::with_capacity(self.sources.len());
        for source in &self.sources {
            if source.id.is_empty() {
                return Err(ConfigError::Validation(
                    "a source has an empty id".to_owned(),
                ));
            }
            if !seen_sources.insert(source.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate source id {:?}",
                    source.id
                )));
            }
        }
        let mut seen_cells: HashSet<&str> = HashSet::with_capacity(self.cells.len());
        for cell in &self.cells {
            if cell.id.is_empty() {
                return Err(ConfigError::Validation("a cell has an empty id".to_owned()));
            }
            if !seen_cells.insert(cell.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate cell id {:?}",
                    cell.id
                )));
            }
        }
        Ok(())
    }

    /// Ensure every `cells.source.input_id` resolves to a declared source.
    fn validate_cell_bindings(&self) -> Result<(), ConfigError> {
        let source_ids: HashSet<&str> = self.sources.iter().map(|s| s.id.as_str()).collect();
        for cell in &self.cells {
            if let Some(input_id) = &cell.source.input_id {
                if !source_ids.contains(input_id.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "cell {:?} binds unknown source input_id {input_id:?}",
                        cell.id
                    )));
                }
            } else if cell.source.kind.is_none() {
                // Neither a managed reference nor an inline spec: nothing to render.
                return Err(ConfigError::Validation(format!(
                    "cell {:?} has no source binding (neither input_id nor inline kind)",
                    cell.id
                )));
            }
        }
        Ok(())
    }

    /// Ensure outputs are sane (at least one; each with a non-empty codec where a
    /// codec applies; a non-empty, unique stable id — ADR-0034 / RT-12).
    ///
    /// Output **id uniqueness** is enforced over the *resolved* ids
    /// ([`Output::id`] — the explicit operator id, or the derived
    /// [`Output::label`] for a v1/v2 output that declares none), so two outputs
    /// can never resolve to the same routing handle (an explicit id may not even
    /// collide with another output's derived label id). An explicitly-authored
    /// `id` may not be empty.
    fn validate_outputs(&self) -> Result<(), ConfigError> {
        if self.outputs.is_empty() {
            return Err(ConfigError::Validation(
                "at least one output must be declared".to_owned(),
            ));
        }
        let mut seen_ids: HashSet<String> = HashSet::with_capacity(self.outputs.len());
        for output in &self.outputs {
            output.validate()?;
            let id = output.id();
            if !seen_ids.insert(id.clone()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate output id {id:?} (output ids must be unique)"
                )));
            }
        }
        Ok(())
    }

    /// Validate the optional audio routing block (ADR-R005) and every output's
    /// audio selection against it.
    ///
    /// Enforces (in addition to [`AudioRouting::validate`] and
    /// [`OutputAudio::validate`]): a route may only reference a declared source;
    /// the discrete tracks an output selects must be the ones the routing block
    /// declares (program bus + named tracks). When the document declares no
    /// `[audio]` block, an output that nonetheless selects discrete tracks is
    /// rejected (there is nothing to select from) — an output asking only for the
    /// implicit program bus is fine.
    fn validate_audio(&self) -> Result<(), ConfigError> {
        // The selectable-track universe: the program bus is always available;
        // named discrete tracks come from the routing block (if any).
        let selectable: Vec<&str> = self.selectable_tracks();

        if let Some(routing) = &self.audio {
            let source_ids: Vec<&str> = self.sources.iter().map(|s| s.id.as_str()).collect();
            routing.validate(&source_ids, &selectable)?;
        }

        for output in &self.outputs {
            if let Some(selection) = output.audio() {
                // Cross-check the selection against the transport's verified
                // capability matrix (ADR-R005 §4.2): reference consistency plus
                // the discrete-track count the transport can actually deliver.
                selection.validate_against_capability(
                    &output.label(),
                    &selectable,
                    output.audio_capability(),
                )?;
            }
        }

        Ok(())
    }

    /// The document-wide **selectable audio tracks**: the program bus (always
    /// available) plus every named discrete track the `[audio]` block declares.
    /// Shared by audio-selection validation and the audio-routing desugar.
    fn selectable_tracks(&self) -> Vec<&str> {
        match &self.audio {
            Some(routing) => routing.declared_tracks(),
            None => vec![PROGRAM_TRACK_NAME],
        }
    }

    /// The declared **subtitle layer** ids a subtitle crosspoint may target.
    ///
    /// Subtitle layers are overlay layers today; until a dedicated subtitle-layer
    /// model lands, every declared overlay id is an addressable layer. The
    /// desugar derives no subtitle crosspoints (legacy captions are per-source,
    /// not per-layer), so this set only gates an **explicit** routing table.
    fn subtitle_layers(&self) -> HashSet<&str> {
        self.overlays.iter().map(|o| o.id.as_str()).collect()
    }

    /// The declared **output ids** an output crosspoint may target (ADR-0034 /
    /// RT-12).
    ///
    /// Each output is addressed by its stable [`Output::id`] — the explicit
    /// operator id, or the derived [`Output::label`] for a v1/v2 output that
    /// declares none — the same string the desugar emits.
    fn output_ids(&self) -> Vec<String> {
        self.outputs.iter().map(Output::id).collect()
    }

    /// The decoupled-routing **crosspoint table** for this document (ADR-0034 /
    /// RT-4): the explicit `[routing]` table when present, otherwise the
    /// equivalent table **desugared** from the legacy `cells`/`audio`/`outputs`
    /// fields.
    ///
    /// The desugar is the load-bearing back-compat guarantee: a v1/v2 document
    /// (no `[routing]`) and its desugared v3 form solve to **identical** routing
    /// — each `[[cells]]` with a `source.input_id` becomes a
    /// [`VideoCrosspoint`]`{cell, StreamRef{input_id, Video, Best}}`; each
    /// `[audio].routes` entry an [`AudioCrosspoint`]; each `[[outputs]]` an
    /// [`OutputCrosspoint`]`{output, program:"main"}`.
    #[must_use]
    pub fn routing_table(&self) -> RoutingTable {
        match &self.routing {
            Some(table) => table.clone(),
            None => self.desugared_routing_table(),
        }
    }

    /// Derive the equivalent [`RoutingTable`] from the legacy fields (the
    /// desugar). See [`MultiviewConfig::routing_table`].
    fn desugared_routing_table(&self) -> RoutingTable {
        let mut table = RoutingTable::default();

        // VIDEO: each cell with a managed input_id → a Video/Best crosspoint.
        // A cell with only an inline (`kind`) source has no managed input id to
        // address, so it carries no crosspoint (it is not a router source).
        for cell in &self.cells {
            if let Some(input_id) = &cell.source.input_id {
                table.video.push(VideoCrosspoint {
                    cell: cell.id.clone(),
                    source: StreamRef::best(input_id.clone(), CoreStreamKind::Video),
                });
            }
        }

        // AUDIO: each declared route → an audio crosspoint, keyed by its
        // destination (the named discrete track, or the program bus when the
        // route names none), carrying the route's gain/mute. This composes with
        // the `[audio]` block (ADR-R005) — it is the per-stream view of the same
        // routing, not a second model.
        if let Some(routing) = &self.audio {
            for route in &routing.routes {
                let target = route
                    .target_track
                    .clone()
                    .unwrap_or_else(|| PROGRAM_TRACK_NAME.to_owned());
                table.audio.push(AudioCrosspoint {
                    target,
                    source: StreamRef::best(route.input_id.clone(), CoreStreamKind::Audio),
                    gain_db: route.gain_db,
                    mute: route.mute,
                });
            }
        }

        // OUTPUT: each declared output → an output crosspoint on the single
        // "main" program (until ADR-0030's ProgramSet lands), addressed by the
        // output's stable id (explicit operator id, or the derived label — RT-12).
        for output in &self.outputs {
            table.output.push(OutputCrosspoint {
                output: output.id(),
                program: MAIN_PROGRAM.to_owned(),
            });
        }

        // SUBTITLE: legacy captions are per-source (CaptionSelector), not
        // per-layer crosspoints, so the desugar derives none. An explicit
        // routing table is where per-layer subtitle breakaway is expressed.

        table
    }

    /// Validate the decoupled-routing crosspoint table (ADR-0034 / RT-4).
    ///
    /// When an explicit `[routing]` table is present it is validated against the
    /// document's declared sources/cells/tracks/layers/outputs (structural
    /// references only — `Language`/`Index` selector resolution is deferred to
    /// admission, so an unresolved language is **not** a config error), and it is
    /// checked for **consistency** with the legacy cell-bindings: a video
    /// crosspoint may not contradict a `[[cells]]` binding of the same cell
    /// (mirroring ADR-0030's rejection of an inconsistent both-populated
    /// document). An absent table desugars and is trivially consistent.
    fn validate_routing(&self) -> Result<(), ConfigError> {
        let Some(table) = &self.routing else {
            return Ok(());
        };

        let tracks = self.selectable_tracks();
        let output_ids = self.output_ids();
        let refs = RoutingRefs {
            sources: self.sources.iter().map(|s| s.id.as_str()).collect(),
            cells: self.cell_ids(),
            tracks: tracks.iter().copied().collect(),
            layers: self.subtitle_layers(),
            outputs: output_ids.iter().map(String::as_str).collect(),
        };
        table.validate(&refs)?;

        // Consistency with the legacy cell-bindings: where BOTH a `[[cells]]`
        // binding and an explicit video crosspoint name the same cell, they must
        // agree on the source input. A contradiction is the inconsistent
        // both-populated case ADR-0030 rejects.
        for xp in &table.video {
            for cell in &self.cells {
                if cell.id == xp.cell {
                    if let Some(legacy_input) = &cell.source.input_id {
                        if legacy_input != &xp.source.input_id {
                            return Err(ConfigError::Validation(format!(
                                "cell {:?} is bound to source {legacy_input:?} by [[cells]] but to \
                                 {:?} by an explicit routing crosspoint (inconsistent \
                                 both-populated document)",
                                xp.cell, xp.source.input_id
                            )));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Validate the optional placement policy block (ADR-0018): reserve-headroom
    /// in range, weights non-negative, migration policy sane. Absent ⇒ the
    /// engine's conservative built-in defaults apply (nothing to validate).
    fn validate_placement(&self) -> Result<(), ConfigError> {
        if let Some(placement) = &self.placement {
            placement.validate()?;
        }
        Ok(())
    }

    /// The set of declared cell ids, for reference validation.
    fn cell_ids(&self) -> HashSet<&str> {
        self.cells.iter().map(|c| c.id.as_str()).collect()
    }

    /// Validate probes: each is internally consistent, ids are unique, and every
    /// watched cell resolves to a declared cell.
    fn validate_probes(&self) -> Result<(), ConfigError> {
        let cells = self.cell_ids();
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.probes.len());
        for probe in &self.probes {
            probe.validate()?;
            if !seen.insert(probe.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate probe id {:?}",
                    probe.id
                )));
            }
            if !cells.contains(probe.cell.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "probe {:?} watches unknown cell {:?}",
                    probe.id, probe.cell
                )));
            }
        }
        Ok(())
    }

    /// Validate tally profiles: each is internally consistent, ids are unique,
    /// and every index→cell mapping resolves to a declared cell.
    fn validate_tally_profiles(&self) -> Result<(), ConfigError> {
        let cells = self.cell_ids();
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.tally_profiles.len());
        for profile in &self.tally_profiles {
            profile.validate()?;
            if !seen.insert(profile.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate tally profile id {:?}",
                    profile.id
                )));
            }
            for rule in &profile.index_cells {
                if !cells.contains(rule.cell.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "tally profile {:?} maps index {} to unknown cell {:?}",
                        profile.id, rule.index, rule.cell
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validate salvos: each is internally consistent, ids are unique, and every
    /// referenced cell/source resolves.
    fn validate_salvos(&self) -> Result<(), ConfigError> {
        let cells = self.cell_ids();
        let sources: HashSet<&str> = self.sources.iter().map(|s| s.id.as_str()).collect();
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.salvos.len());
        for salvo in &self.salvos {
            salvo.validate()?;
            if !seen.insert(salvo.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate salvo id {:?}",
                    salvo.id
                )));
            }
            for recall in &salvo.sources {
                if !cells.contains(recall.cell.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "salvo {:?} rebinds unknown cell {:?}",
                        salvo.id, recall.cell
                    )));
                }
                if !sources.contains(recall.input_id.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "salvo {:?} binds unknown source {:?}",
                        salvo.id, recall.input_id
                    )));
                }
            }
            for recall in &salvo.tally {
                if !cells.contains(recall.cell.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "salvo {:?} sets tally on unknown cell {:?}",
                        salvo.id, recall.cell
                    )));
                }
            }
            for recall in &salvo.umd {
                if !cells.contains(recall.cell.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "salvo {:?} sets umd on unknown cell {:?}",
                        salvo.id, recall.cell
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validate video walls: ids unique across walls, and each passes the core
    /// wall invariants.
    fn validate_walls(&self) -> Result<(), ConfigError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.walls.len());
        for wall in &self.walls {
            if !seen.insert(wall.name.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate wall name {:?}",
                    wall.name
                )));
            }
            wall.validate()?;
        }
        Ok(())
    }
}

/// Map a schema fit token onto the core [`FitMode`] (defaulting to `Contain`).
///
/// The schema vocabulary (`fill`/`contain`/`cover`/`none`/`scale_down`) is the
/// CSS `object-fit` set; the core model carries the three blend-relevant modes,
/// so `none`/`scale_down` (which letterbox without stretching) map to
/// [`FitMode::Contain`].
fn parse_fit(fit: Option<&str>) -> FitMode {
    match fit {
        Some("cover") => FitMode::Cover,
        Some("fill") => FitMode::Fill,
        _ => FitMode::Contain,
    }
}

impl Source {
    /// Validate this source's per-item semantics — the same checks
    /// [`MultiviewConfig::validate`] applies per source (id non-empty, GPU pin,
    /// solid hex colour, clock timezone bounds). Document-level rules
    /// (id uniqueness, cell references) remain on the document.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the violated rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.is_empty() {
            return Err(ConfigError::Validation(
                "a source has an empty id".to_owned(),
            ));
        }
        if let Some(pin) = &self.gpu_pin {
            pin.validate().map_err(|e| {
                ConfigError::Validation(format!("source {:?} gpu_pin: {e}", self.id))
            })?;
        }
        match &self.kind {
            SourceKind::Solid { color } => {
                if parse_hex_color(color).is_none() {
                    return Err(ConfigError::Validation(format!(
                        "source {:?}: solid color {color:?} is not a #RGB / #RRGGBB hex color",
                        self.id
                    )));
                }
            }
            SourceKind::Clock {
                timezone,
                tz_offset_minutes,
                ..
            } => {
                match timezone {
                    // An IANA zone is authoritative (DST-resolved at render); it
                    // must be a known id. The fixed `tz_offset_minutes` is ignored
                    // when a zone is set, so its range is NOT checked here (a
                    // both-present clock warns via `clock_warnings`, never errors).
                    Some(tz) => {
                        if multiview_overlay::clock::parse_tz(tz).is_none() {
                            return Err(ConfigError::Validation(format!(
                                "source {:?}: timezone {tz:?} is not a known IANA timezone id \
                                 (e.g. \"Australia/Sydney\", \"America/New_York\", \"UTC\")",
                                self.id
                            )));
                        }
                    }
                    // No zone ⇒ the fixed offset is authoritative; range-check it.
                    None => {
                        if !(-720..=840).contains(tz_offset_minutes) {
                            return Err(ConfigError::Validation(format!(
                                "source {:?}: clock tz_offset_minutes {tz_offset_minutes} is out \
                                 of range (real UTC offsets span -720..=840)",
                                self.id
                            )));
                        }
                    }
                }
            }
            SourceKind::Timer {
                target,
                on_target,
                ..
            } => {
                // The target's `at` must parse and any IANA `timezone` must be a
                // known id (typed error, never a panic — ADR-0047 §5.2).
                target.validate().map_err(|e| {
                    ConfigError::Validation(format!("source {:?}: {e}", self.id))
                })?;
                // `recur` is only meaningful for a recurring time-of-day target;
                // reject it on a `date_time` or a non-recurring time-of-day so the
                // configuration is unambiguous (it would otherwise silently
                // degrade to `hold`).
                if matches!(on_target, crate::timer::TimerOnTarget::Recur) {
                    let recurs = matches!(
                        target,
                        crate::timer::TimerTarget::TimeOfDay {
                            recur_daily: true,
                            ..
                        }
                    );
                    if !recurs {
                        return Err(ConfigError::Validation(format!(
                            "source {:?}: on_target = \"recur\" requires a time_of_day target \
                             with recur_daily = true",
                            self.id
                        )));
                    }
                }
            }
            // Network/synthetic kinds carry no kind-specific field that can be
            // validated structurally here (a URL's reachability is a runtime
            // concern, not a config one). The `youtube` URL is resolved at
            // ingest by an external `yt-dlp` (ADR-0015), so like the other URL
            // kinds it is accepted as-is at config time.
            SourceKind::Bars
            | SourceKind::Rtsp { .. }
            | SourceKind::Hls { .. }
            | SourceKind::Youtube { .. }
            | SourceKind::Ts { .. }
            | SourceKind::Srt { .. }
            | SourceKind::Rtmp { .. }
            | SourceKind::Ndi { .. }
            | SourceKind::File { .. }
            // AES67's binding (SDP / multicast group / PTP domain) is a runtime
            // ingest concern, like the network URL kinds — accepted as authored.
            | SourceKind::Aes67 { .. } => {}
        }
        Ok(())
    }

    /// Non-fatal advisories for this source's clock configuration (empty for a
    /// non-clock source or a clock with no conflicts).
    ///
    /// The one current advisory: when a clock sets **both** `timezone` (IANA) and
    /// `tz_offset_minutes`, the IANA zone wins (DST-correct per instant) and the
    /// fixed offset is ignored — both-present is legal, never an error.
    ///
    /// Surfacing: `multiview validate` renders each advisory as a `WARN` line in
    /// its report, and `multiview run` emits each via `tracing::warn!` at startup
    /// (both through the cli's `config_warnings` collector). The control API's
    /// typed validation rejects invalid bodies but carries no warnings channel,
    /// so advisories are not part of REST responses today.
    #[must_use]
    pub fn clock_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let SourceKind::Clock {
            timezone: Some(tz),
            tz_offset_minutes,
            ..
        } = &self.kind
        {
            if *tz_offset_minutes != 0 {
                out.push(format!(
                    "source {:?}: both timezone ({tz:?}) and tz_offset_minutes \
                     ({tz_offset_minutes}) are set; timezone wins (DST-correct) and \
                     tz_offset_minutes is ignored",
                    self.id
                ));
            }
        }
        out
    }
}

impl Output {
    /// Validate this output's per-item semantics — the same checks
    /// [`MultiviewConfig::validate`] applies per output (non-empty codec where
    /// the kind carries one, non-empty explicit id, GPU pin). Document-level
    /// rules (id uniqueness, audio track resolution) remain on the document.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the violated rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let codec = match self {
            Output::RtspServer { codec, .. }
            | Output::LlHls { codec, .. }
            | Output::Hls { codec, .. }
            | Output::Rtmp { codec, .. }
            | Output::Srt { codec, .. } => Some(codec),
            // NDI carries a channel-map, AES67 sends raw PCM, and a display
            // head scans out raw frames — none has a (video) codec to validate.
            Output::Ndi { .. } | Output::Aes67 { .. } | Output::Display { .. } => None,
        };
        if let Some(codec) = codec {
            if codec.is_empty() {
                return Err(ConfigError::Validation(
                    "an output declares an empty codec".to_owned(),
                ));
            }
        }
        if let Some(explicit) = self.explicit_id() {
            if explicit.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "output {:?} declares an empty id",
                    self.label()
                )));
            }
        }
        if let Some(pin) = self.gpu_pin() {
            pin.validate()
                .map_err(|e| ConfigError::Validation(format!("output gpu_pin: {e}")))?;
        }
        if let Output::Display {
            connector,
            mode,
            forced_mode,
            ..
        } = self
        {
            validate_display_output(connector, mode.as_ref(), forced_mode.as_ref())?;
        }
        Ok(())
    }
}

/// Validate an [`Output::Display`]'s connector + mode block (ADR-0044):
/// non-empty connector, `mode`/`forced_mode` mutual exclusion (an explicit
/// EDID override and an EDID-less forced timing are contradictory requests),
/// and a structurally sound [`DisplayModeSpec`] wherever one is given.
fn validate_display_output(
    connector: &str,
    mode: Option<&DisplayModeSpec>,
    forced_mode: Option<&DisplayModeSpec>,
) -> Result<(), ConfigError> {
    if connector.trim().is_empty() {
        return Err(ConfigError::Validation(
            "output display: connector must not be empty (use \"auto\" for the first \
             connected connector)"
                .to_owned(),
        ));
    }
    if mode.is_some() && forced_mode.is_some() {
        return Err(ConfigError::Validation(format!(
            "output \"display {connector}\": `mode` and `forced_mode` are mutually \
             exclusive (`mode` overrides among EDID modes; `forced_mode` is the \
             CVT-RB timing for an EDID-less head)"
        )));
    }
    for (field, spec) in [("mode", mode), ("forced_mode", forced_mode)] {
        let Some(spec) = spec else { continue };
        if spec.width == 0 {
            return Err(ConfigError::Validation(format!(
                "output \"display {connector}\": {field}.width must be positive"
            )));
        }
        if spec.height == 0 {
            return Err(ConfigError::Validation(format!(
                "output \"display {connector}\": {field}.height must be positive"
            )));
        }
        let refresh = spec.refresh.rational();
        if !refresh.is_valid() || refresh.num <= 0 || refresh.den <= 0 {
            return Err(ConfigError::Validation(format!(
                "output \"display {connector}\": {field}.refresh must be a positive \
                 exact rational (e.g. \"60000/1001\"), got {refresh:?}"
            )));
        }
    }
    Ok(())
}
