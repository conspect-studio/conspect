//! The multi-program configuration types (ADR-0030, MP-0).
//!
//! Today the engine runs exactly **one** program: one fixed-cadence output clock
//! drives one composited canvas per tick, encoded once and fanned to N
//! transports. ADR-0030 introduces the **`Program`** abstraction so the engine
//! can eventually run N concurrent, independently start/stoppable output
//! pipelines, each one a multiview composite (today), a guarded passthrough
//! (MP-3), or a transcode (MP-4).
//!
//! This module carries the **config-layer** program types only — the identity
//! ([`ProgramId`]), the tagged kind ([`ProgramKind`]), and the per-program
//! specification ([`ProgramSpec`]). MP-0 wires these into the existing
//! single-program run path: the CLI derives one [`ProgramSpec`] (`id = "main"`,
//! [`ProgramKind::Multiview`]) from the legacy top-level
//! `canvas`/`layout`/`cells`/`overlays`/`outputs` block and the engine's
//! `MultiviewProgram` is constructed from it — so the run path flows through one
//! `Program` with **zero** behavioural change. The schema-root
//! `programs: Vec<ProgramSpec>` field, the backward-compat desugaring, and the
//! per-program output-label cross-validation arrive in MP-5; this module does not
//! change [`MultiviewConfig`](crate::MultiviewConfig).
//!
//! All unions are **internally tagged** by `kind` (never `untagged`) per ADR-0010
//! / conventions §5, so they round-trip robustly across TOML and JSON.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::failover::{default_failover_slate, FailoverSlate};
use crate::schema::{Canvas, Cell, Layout, Output, Overlay};

/// The canonical identity of one program within a program set (ADR-0030). It
/// keys the `ProgramId → ProgramHandle` map the engine's supervisor will own
/// (MP-1), scopes the realtime envelope (`envelope.id = "{program}/{output}"`,
/// MP-6), and carries the per-program context
/// [`PipelineError`](../../multiview_cli/pipeline) grows in MP-0.
///
/// The legacy single-program run desugars to the reserved id [`ProgramId::MAIN`]
/// (`"main"`) so the existing config keeps working unchanged (ADR-0030 §6).
///
/// An id is a non-empty, trimmed token: validated on construction via
/// [`ProgramId::new`] / [`TryFrom`] so a malformed id is rejected at the config
/// boundary rather than surfacing as a dangling map key downstream.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProgramId(String);

impl ProgramId {
    /// The reserved id of the implicit program the legacy top-level
    /// `canvas`/`layout`/`cells`/`overlays`/`outputs` block desugars to
    /// (ADR-0030 §6). The single-program run path uses this id.
    pub const MAIN: &'static str = "main";

    /// Build a validated [`ProgramId`] from a borrowed token.
    ///
    /// The id is trimmed of surrounding whitespace and must be non-empty after
    /// trimming.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] if `id` is empty or whitespace-only.
    pub fn new(id: impl Into<String>) -> Result<Self, ConfigError> {
        let id = id.into();
        let trimmed = id.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::Validation(
                "program id must be a non-empty token".to_owned(),
            ));
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// The reserved [`ProgramId::MAIN`] (`"main"`) identity the single-program
    /// legacy run path uses.
    #[must_use]
    pub fn main() -> Self {
        // `MAIN` is a non-empty literal, so this constructs the reserved id
        // directly rather than threading a `Result` through every infallible
        // call site (no `unwrap`, no panic on the path).
        Self(Self::MAIN.to_owned())
    }

    /// Borrow the id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProgramId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProgramId {
    type Error = ConfigError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ProgramId {
    type Error = ConfigError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProgramId> for String {
    fn from(id: ProgramId) -> Self {
        id.0
    }
}

/// What one [`ProgramSpec`] does — the pipeline shape, internally tagged by
/// `kind` (ADR-0030 §1, conventions §5 — never `untagged`).
///
/// `#[non_exhaustive]`: only [`ProgramKind::Multiview`] (today's behaviour) is
/// populated in MP-0. The guarded-passthrough and transcode kinds land with
/// their own slices (MP-3 → GP-0…GP-12, and MP-4); they are deliberately **not**
/// present here yet — no stub or panicking variant — so the enum only ever names
/// a kind it can actually run. Downstream `match` statements carry a wildcard arm
/// because of `#[non_exhaustive]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProgramKind {
    /// Composite many shared sources into one canvas — today's behaviour. Owns
    /// its own canvas + layout + cells + overlays (its fps drives the
    /// per-program output cadence), referencing the **top-level shared**
    /// `sources` by `input_id` (decode-once-use-many, MP-2). The output sinks
    /// the canvas is fanned to live on the enclosing [`ProgramSpec::outputs`].
    Multiview {
        /// The output canvas (geometry + fps → this program's cadence).
        canvas: Canvas,
        /// The layout placement strategy (preset or CSS-grid).
        layout: Layout,
        /// Cells (tiles) and their source bindings.
        #[serde(default)]
        cells: Vec<Cell>,
        /// Overlay layers composited over the canvas.
        #[serde(default)]
        overlays: Vec<Overlay>,
    },
}

impl ProgramKind {
    /// The static tag (`"multiview"`, …) of this kind — the realtime
    /// `ProgramKindTag` (MP-6) and the management plane's per-program badge map
    /// onto this. Kept in sync with the serde `tag = "kind"` discriminant.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Multiview { .. } => "multiview",
        }
    }
}

/// One program: an `id` + optional `display_name` + `autostart` flag + the
/// tagged [`ProgramKind`] + its own output sinks (ADR-0030 §1/§5.1).
///
/// In MP-0 the CLI synthesizes exactly one of these (`id = "main"`,
/// [`ProgramKind::Multiview`]) from the legacy config block via
/// [`ProgramSpec::main_multiview`], and the engine's `MultiviewProgram` is built
/// from it — the single seam through which the existing run path now flows. The
/// `programs: Vec<ProgramSpec>` schema root + backward-compat desugaring arrive
/// in MP-5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProgramSpec {
    /// This program's identity (unique within a program set).
    pub id: ProgramId,
    /// An optional human-facing name for the management UI. Absent ⇒ the UI
    /// shows the [`id`](ProgramSpec::id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether the program set starts this program automatically on engine
    /// start. Defaults to `true` (the legacy single program autostarts).
    #[serde(default = "default_autostart")]
    pub autostart: bool,
    /// What this program shows on **source loss** — the configurable
    /// failover-slate policy (ADR-0030 §4), selected the **same way** as a layout
    /// tile ([`Cell::on_loss`]). For the non-layout **passthrough / transcode**
    /// case this is the program-level slate the pre-baked GP-4 slate displays on
    /// input loss; for a [`ProgramKind::Multiview`] program it is the
    /// whole-canvas fallback when every tile is down. Defaults to
    /// [`FailoverSlate::Bars`] (the broadcast standard) when omitted, so a
    /// pre-existing program keeps working and gets the default.
    #[serde(default = "default_failover_slate")]
    pub on_loss: FailoverSlate,
    /// What this program is (multiview composite today).
    #[serde(flatten)]
    pub kind: ProgramKind,
    /// The output sinks this program's canvas is fanned to (reusing the existing
    /// [`Output`] enum verbatim, ADR-0030 §5.1).
    #[serde(default)]
    pub outputs: Vec<Output>,
}

impl ProgramSpec {
    /// Synthesize the implicit `"main"` [`ProgramKind::Multiview`] program from
    /// the legacy top-level config block (ADR-0030 §6 desugaring), used by the
    /// MP-0 single-program run path.
    ///
    /// The caller passes the already-parsed `canvas`/`layout`/`cells`/
    /// `overlays`/`outputs`; this only assembles them under the reserved
    /// [`ProgramId::MAIN`] identity — it performs no solving or validation (that
    /// stays where it is in [`MultiviewConfig`](crate::MultiviewConfig)).
    #[must_use]
    pub fn main_multiview(
        canvas: Canvas,
        layout: Layout,
        cells: Vec<Cell>,
        overlays: Vec<Overlay>,
        outputs: Vec<Output>,
    ) -> Self {
        Self {
            id: ProgramId::main(),
            display_name: None,
            autostart: true,
            on_loss: default_failover_slate(),
            kind: ProgramKind::Multiview {
                canvas,
                layout,
                cells,
                overlays,
            },
            outputs,
        }
    }
}

/// The serde default for [`ProgramSpec::autostart`]: the legacy single program
/// autostarts.
const fn default_autostart() -> bool {
    true
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::schema::{CanvasColor, Fps};

    fn sample_canvas() -> Canvas {
        Canvas {
            width: 1920,
            height: 1080,
            fps: "25/1".parse::<Fps>().unwrap(),
            pixel_format: "nv12".to_owned(),
            background: "#101014".to_owned(),
            color: CanvasColor {
                profile: "sdr-bt709-limited".to_owned(),
                primaries: None,
                transfer: None,
                matrix: None,
                range: None,
            },
        }
    }

    #[test]
    fn program_id_rejects_empty_and_trims() {
        assert!(ProgramId::new("").is_err());
        assert!(ProgramId::new("   ").is_err());
        let id = ProgramId::new("  main  ").unwrap();
        assert_eq!(id.as_str(), "main");
    }

    #[test]
    fn program_id_main_is_reserved_token() {
        assert_eq!(ProgramId::main().as_str(), ProgramId::MAIN);
        assert_eq!(ProgramId::main().to_string(), "main");
    }

    #[test]
    fn program_id_serializes_as_bare_string() {
        let id = ProgramId::new("studio-a").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"studio-a\"");
        let back: ProgramId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn program_id_deserialize_rejects_empty() {
        let err = serde_json::from_str::<ProgramId>("\"\"");
        assert!(err.is_err());
    }

    #[test]
    fn multiview_kind_tag_is_stable() {
        let kind = ProgramKind::Multiview {
            canvas: sample_canvas(),
            layout: Layout::Preset {
                preset: "2x2".to_owned(),
            },
            cells: Vec::new(),
            overlays: Vec::new(),
        };
        assert_eq!(kind.tag(), "multiview");
    }

    #[test]
    fn main_multiview_uses_reserved_id_and_autostarts() {
        let spec = ProgramSpec::main_multiview(
            sample_canvas(),
            Layout::Preset {
                preset: "2x2".to_owned(),
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(spec.id, ProgramId::main());
        assert!(spec.autostart);
        assert_eq!(spec.kind.tag(), "multiview");
    }

    #[test]
    fn program_spec_round_trips_internally_tagged() {
        let spec = ProgramSpec::main_multiview(
            sample_canvas(),
            Layout::Preset {
                preset: "3x3".to_owned(),
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let json = serde_json::to_string(&spec).unwrap();
        // Internally tagged: the `kind` discriminant is a sibling field, never an
        // `untagged` shape (ADR-0010).
        assert!(json.contains("\"kind\":\"multiview\""));
        let back: ProgramSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }
}
